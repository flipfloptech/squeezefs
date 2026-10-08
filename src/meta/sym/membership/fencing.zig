//! Fencing under the symmetric plane (charter §5.5 "Fencing"; `docs/
//! porting/sym_wire_membership_recovery.md` §4): the metadata namespace
//! holds WERO (rtype 3) by the MANAGER with every appender a REGISTRANT
//! under ONE key per namespace class — the ledger's preempt of that key
//! fences the member everywhere. A joiner NEVER acquires: a co-located
//! mount (the manager's D0 flock or claim boot is this host's — one head,
//! one association, the device sees ONE host) ADOPTS with ZERO device
//! mutation, the standing holder cross-checked against the enrolled keys
//! (a register here destroyed the fence in the rung-9 finding: `rtype 0,
//! regctl 0`); a remote one registers and walks the CAP GATE — the report
//! read in two steps sized by REGCTL, the cap in force = declared else
//! LEARNED else unbounded, learned ONLY from the REPEAT of a device-
//! answered command-/vendor-specific status at the same regctl, unlearned
//! on a later success, refused LOUD at or past it naming the remedy.
//!
//! `writer_term` is the per-volume durable era: written and barriered at
//! the mount gate, bumped at every (re)join, NEVER deleted (the claim is;
//! the era ladder must outlive it); every fencing token is `(term << 40) |
//! seq`; `recovered_by_term` names the recoverer.
//!
//! The frame screen's four rules are the loader's (`bset.FrameScreen`,
//! `node.verifyNodeExtentScreened`); this module supplies its INPUTS: the
//! slot's current `(g, lessee)` from the lease table, and the Recovering
//! SUSPENSION — `appender_current = null` for a dead lessee's slots from
//! the recovery's step 2 to its step 8 (rule 4 inert, rules 1–3 armed).
//! The `(0, 0)` PRIMING law: every writer's open primes its frame stamps
//! at its FIRST frame writer (the claim gate's ring-recovery preflight
//! too); a stamp asked of a leased slot before the prime is the tripwire
//! `appender_unstamped_frame` (a `(0, 0)` frame on a leased leaf ENDS THE
//! LOG at the next load — an acked-writes loss class found twice).
//! Co-located mounts sharing one registrant are their own guarantee row:
//! `flock` + `T_self` + the frame screen; the recovery's own-key guard and
//! adopt-not-register are its two faces.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const log = @import("../../../core/log.zig");
const pr = @import("../../guard/pr.zig");
const nvme = @import("../../guard/nvme_cmd.zig");
const bset = @import("../../bset.zig");
const lease_table = @import("../slots/lease_table.zig");

pub const FrameScreen = bset.FrameScreen;
pub const FrameStamp = bset.FrameStamp;
pub const LeaseTable = lease_table.LeaseTable;

pub const stats = struct {
    pub var pr_registrants_per_namespace: counters.Gauge = .{};
    pub var pr_report_bytes: counters.Gauge = .{};
    /// The cap in force (declared else learned else 0 = unbounded).
    pub var pr_registrant_cap: counters.Gauge = .{};
    /// Refused at or past the cap (0 on nvmet by construction).
    pub var pr_registrant_cap_refusals: counters.ShardedCounter = .{};
    /// Another rkey under OUR wire host id (two mounts on one registrant).
    pub var pr_registrant_shared: counters.Gauge = .{};
    pub var pr_wero_acquires: counters.ShardedCounter = .{};
    pub var pr_registrant_joins: counters.ShardedCounter = .{};
    pub var pr_colocated_adoptions: counters.ShardedCounter = .{};
    /// The death preempt landed (meta + data namespaces).
    pub var appender_recovery_preempts: counters.ShardedCounter = .{};
    /// A preempt withheld because the dead key is THIS host's registrant.
    pub var appender_recovery_own_key_guards: counters.ShardedCounter = .{};
    /// The per-volume era in force.
    pub var writer_term: counters.Gauge = .{};
    /// Slots whose lessee's page is `Recovering` (rule 4 suspended).
    pub var fence_recovering_suspensions: counters.Gauge = .{};
    /// A `(0, 0)` stamp asked of a leased slot before the prime: must
    /// stay 0.
    pub var appender_unstamped_frame: counters.Tripwire = .init("appender_unstamped_frame");
};

// ---------------------------------------------------------------------------
// The WERO duty split.
// ---------------------------------------------------------------------------

pub const JoinError = error{
    /// At or past the registrant cap in force.
    RegistrantCap,
    /// A co-located join found a holder none of the enrolled keys name.
    HolderNotEnrolled,
    /// A co-located join found no reservation at all (the manager holds
    /// none: the fence is down).
    NoHolder,
    /// The manager holds rtype 1 (Write Exclusive): a flat mount's hold.
    NotWero,
    Device,
};

/// The manager's hold: WERO under our key after the register ladder.
pub fn acquireWero(client: *pr.Client) pr.Error!void {
    client.rtype = nvme.RTYPE_WERO;
    _ = try client.registerLadder();
    try client.acquire();
    stats.pr_wero_acquires.inc();
    // The gauges read the namespace at every gate: the report after the hold.
    var rep = try client.report();
    rep.deinit(client.gpa);
    noteReport(client);
}

fn noteReport(client: *const pr.Client) void {
    stats.pr_registrants_per_namespace.set(client.regctl);
    stats.pr_report_bytes.set(client.report_bytes);
}

/// The cap gate (R:429-453): `cap_in_force = 0` is unbounded; else a
/// count at or past it refuses loud.
pub fn registrantCapGate(regctl: u16, cap_in_force: u16) JoinError!void {
    if (cap_in_force != 0 and regctl >= cap_in_force) {
        stats.pr_registrant_cap_refusals.inc();
        return error.RegistrantCap;
    }
}

pub fn writeCapRefusal(w: *std.Io.Writer, namespace: []const u8, regctl: u16, cap: u16) std.Io.Writer.Error!void {
    try w.print("{s}: {d} registrant(s) at the namespace's cap of {d} — refusing the join loud. Put the kernel nvmet target in front of the array (its registrant list is unbounded, the ONE supported target), or use fewer hosts per namespace", .{ namespace, regctl, cap });
}

pub const Registered = struct {
    regctl: u16,
    cap_in_force: u16,
    /// Another rkey under our wire host id.
    shared: bool,
};

/// A REMOTE joiner: register under ONE key, read the report, feed the
/// learner, run the gate against the cap in force. The manager's hold must
/// be WERO (a flat mount's rtype 1 refuses the join).
pub fn joinAsRegistrant(client: *pr.Client, declared_cap: ?u16) JoinError!Registered {
    client.rtype = nvme.RTYPE_WERO;
    const outcome = client.registerLadder() catch |e| switch (e) {
        error.DeviceStatus => {
            // The ladder fed the learner from this status; the gate reads
            // the cap it produced (a repeat learns, one refusal is a
            // candidate — the join still fails on this device answer).
            const cap = client.cap.effective(declared_cap);
            stats.pr_registrant_cap.set(cap);
            if (nvme.isCapLearnable(client.last_status)) {
                stats.pr_registrant_cap_refusals.inc();
                return error.RegistrantCap;
            }
            return error.Device;
        },
        else => return error.Device,
    };
    _ = outcome;
    var rep = client.report() catch return error.Device;
    defer rep.deinit(client.gpa);
    noteReport(client);
    if (rep.rtype != 0 and rep.rtype != nvme.RTYPE_WERO) return error.NotWero;
    const cap = client.cap.effective(declared_cap);
    stats.pr_registrant_cap.set(cap);
    registrantCapGate(rep.regctl, cap) catch |e| {
        // Zero residue: the refused registrant leaves the table.
        client.unregister(client.key) catch {};
        return e;
    };
    const shared = client.identityShared() catch false;
    stats.pr_registrant_shared.set(@intFromBool(shared));
    stats.pr_registrant_joins.inc();
    return .{ .regctl = rep.regctl, .cap_in_force = cap, .shared = shared };
}

/// A CO-LOCATED joiner: ZERO device mutation — the standing holder read
/// and cross-checked against the enrolled keys (the manager's claim-
/// derived key on the metadata namespace; the roster's writer keys on a
/// data namespace). Answers the holder's key.
pub fn adoptColocated(client: *pr.Client, enrolled: []const u64) JoinError!u64 {
    var rep = client.report() catch return error.Device;
    defer rep.deinit(client.gpa);
    noteReport(client);
    const holder = rep.holder() orelse return error.NoHolder;
    if (rep.rtype != nvme.RTYPE_WERO) return error.NotWero;
    for (enrolled) |k| if (k == holder) {
        stats.pr_colocated_adoptions.inc();
        return holder;
    };
    return error.HolderNotEnrolled;
}

pub const PreemptOutcome = enum {
    /// The preempt-and-abort LANDED: `pr_fenced`, the tail scan skipped.
    fenced,
    /// The dead key is THIS host's registrant (a co-located appender): a
    /// preempt would take down our own fence — the full tail scan is the
    /// fence.
    own_key_guard,
    /// A key-less record (`appender clear`'s, an unvalidated word): the
    /// full tail scan.
    no_key,
    /// No device fence on this substrate (a regular file: the single-
    /// kernel class) — the loud posture, the full tail scan.
    no_device,
    /// The device refused: the full tail scan.
    failed,
};

/// §5.9 step 1: preempt-and-abort the dead registrant's key on the
/// namespace this client holds (`RACQA 2`; nvmet runs it as a plain
/// preempt). `pr_fenced ≡ fenced`.
pub fn preemptDeadRegistrant(client: ?*pr.Client, victim_key: u64) PreemptOutcome {
    const c = client orelse return .no_device;
    if (victim_key == 0) return .no_key;
    if (victim_key == c.key) {
        stats.appender_recovery_own_key_guards.inc();
        return .own_key_guard;
    }
    c.preemptAndAbort(victim_key) catch |e| {
        var buf: [128]u8 = undefined;
        var w: std.Io.Writer = .fixed(&buf);
        c.writeError(&w, e) catch {};
        log.warn("recovery preempt: preempt-and-abort of key 0x{x} did not land ({s}) — the full tail scan is the fence", .{ victim_key, w.buffered() });
        return .failed;
    };
    stats.appender_recovery_preempts.inc();
    return .fenced;
}

// ---------------------------------------------------------------------------
// `writer_term`.
// ---------------------------------------------------------------------------

/// The token's seq width: `(term << 40) | seq` — 2^40 seqs per era, 2^24
/// eras (B:783-789).
pub const TOKEN_SEQ_BITS: u6 = 40;

pub fn fencingToken(term: u64, seq: u64) u64 {
    std.debug.assert(seq < (@as(u64, 1) << TOKEN_SEQ_BITS));
    return (term << TOKEN_SEQ_BITS) | seq;
}

pub fn tokenTerm(token: u64) u64 {
    return token >> TOKEN_SEQ_BITS;
}

/// The era's durable home (ino 1's `writer_term`, written + barriered by
/// the mount gate).
pub const TermStore = struct {
    ctx: ?*anyopaque,
    load: *const fn (ctx: ?*anyopaque) error{Io}!?u64,
    /// Written and BARRIERED before the caller proceeds.
    store: *const fn (ctx: ?*anyopaque, term: u64) error{Io}!void,
};

pub const WriterTerm = struct {
    term: u64,

    /// The mount gate's bump: the recorded era + 1 (1 on a fresh volume),
    /// durable before anything else of the open — never lowered, never
    /// deleted.
    pub fn arm(store: TermStore) error{Io}!WriterTerm {
        const prior = (try store.load(store.ctx)) orelse 0;
        const term = prior + 1;
        try store.store(store.ctx, term);
        stats.writer_term.set(term);
        return .{ .term = term };
    }

    pub fn token(self: WriterTerm, seq: u64) u64 {
        return fencingToken(self.term, seq);
    }
};

// ---------------------------------------------------------------------------
// The frame screen's inputs and the priming law.
// ---------------------------------------------------------------------------

pub const RecordedTail = struct { g: u32, tail: u32 };

/// The screen for ONE leaf of `slot`, read off the lease table: the
/// current generation and lessee; `appender_current = null` while
/// unleased (rule 4 inert) or while the lessee's page is `Recovering`
/// (the suspension); `pr_fenced` decides rule 1/4's class.
pub const ScreenSource = struct {
    gpa: Allocator,
    table: *LeaseTable,
    pr_fenced: bool,
    /// The dead lessees mid-recovery (steps 2–8).
    recovering: std.ArrayList(u32) = .empty,

    pub fn deinit(self: *ScreenSource) void {
        self.recovering.deinit(self.gpa);
    }

    pub fn isRecovering(self: *const ScreenSource, appender_id: u32) bool {
        for (self.recovering.items) |a| if (a == appender_id) return true;
        return false;
    }

    /// Step 2: `recovering_lessees.insert(id)`.
    pub fn suspendLessee(self: *ScreenSource, appender_id: u32) Allocator.Error!void {
        if (self.isRecovering(appender_id)) return;
        try self.recovering.append(self.gpa, appender_id);
        stats.fence_recovering_suspensions.add(1);
    }

    /// Step 8: the slots are `Unleased` by the lease alone from here.
    pub fn resumeLessee(self: *ScreenSource, appender_id: u32) void {
        for (self.recovering.items, 0..) |a, i| if (a == appender_id) {
            _ = self.recovering.swapRemove(i);
            stats.fence_recovering_suspensions.sub(1);
            return;
        };
    }

    pub fn screenFor(self: *const ScreenSource, slot: u16, recorded_tail: ?RecordedTail) FrameScreen {
        const v = self.table.view(slot);
        const leased = v.state != .unleased and v.state != .migrating;
        const current: ?u32 = if (leased and !self.isRecovering(v.holder)) v.holder else null;
        return .{ .g_current = v.g, .appender_current = current, .recorded_tail = if (recorded_tail) |t| .{ .g = t.g, .tail = t.tail } else null, .pr_fenced = self.pr_fenced };
    }
};

/// The writer's frame stamps per slot, PRIMED at the open's first frame
/// writer from tree 0's lease population: a re-adopted `Leased{me, g}`
/// stamps `g`; an `Unleased{g}` native slot stamps the arm's `g + 1`; a
/// slot leased to ANOTHER appender is never ours to stamp.
pub const FrameStamps = struct {
    gpa: Allocator,
    me: u32,
    primed: bool = false,
    stamps: std.AutoHashMapUnmanaged(u16, u32) = .empty,

    pub fn init(gpa: Allocator, me: u32) FrameStamps {
        return .{ .gpa = gpa, .me = me };
    }

    pub fn deinit(self: *FrameStamps) void {
        self.stamps.deinit(self.gpa);
    }

    /// `prime_frame_stamps`: the first statement of EVERY writer door
    /// (the gate's preflight, the joined door, `appender clear`'s
    /// bring-up). `native` = the arm's own slots, unleased in tree 0,
    /// about to be leased at `g + 1`.
    pub fn prime(self: *FrameStamps, table: *LeaseTable, held: []const u16, native: []const u16) Allocator.Error!void {
        for (held) |s| {
            const v = table.view(s);
            if (v.leasedBy(self.me)) try self.stamps.put(self.gpa, s, v.g);
        }
        for (native) |s| try self.stamps.put(self.gpa, s, table.gOf(s) + 1);
        self.primed = true;
    }

    /// A lease granted after the prime (a first touch, a transfer).
    pub fn learn(self: *FrameStamps, slot: u16, g: u32) Allocator.Error!void {
        try self.stamps.put(self.gpa, slot, g);
    }

    pub fn forget(self: *FrameStamps, slot: u16) void {
        _ = self.stamps.remove(slot);
    }

    /// The stamp a frame on `slot`'s leaf carries. A stamp asked of a slot
    /// this writer leases before the prime (or without a learned `g`) is
    /// the `(0, 0)` tripwire — the frame is written unstamped exactly as
    /// the reference's bug did, counted and loud, never a panic.
    pub fn stampFor(self: *const FrameStamps, slot: u16, table: *const LeaseTable) FrameStamp {
        if (self.stamps.get(slot)) |g| return .{ .appender_id = self.me, .g = g };
        const v = table.view(slot);
        if (v.leasedBy(self.me) or !self.primed) {
            stats.appender_unstamped_frame.fire("a frame on a leased slot's leaf asked for its stamp before the writer's prime (the (0, 0) frame ends the log at the next load)");
        }
        return .{};
    }
};

// ---------------------------------------------------------------------------
// Tests: the fake controller, the term, the screen inputs, the prime.
// ---------------------------------------------------------------------------

const testing = std.testing;
const fake = @import("../../guard/fake_nvme.zig");

const host_m: [16]u8 = "manager-host-id1".*;
const host_r: [16]u8 = "remote-host-id-2".*;
const host_s: [16]u8 = "remote-host-id-3".*;

test "WERO by the manager (rtype 3 on the wire), a remote joiner registers under ONE key and reads the report sized by REGCTL; a co-located joiner adopts with zero device mutation and cross-checks the holder against the enrolled keys; a flat (rtype 1) holder refuses the join" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var mh: fake.Host = .{ .ns = &ns, .host_id = host_m };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    _ = try m.probeRescap();
    try acquireWero(&m);
    try testing.expectEqual(nvme.RTYPE_WERO, ns.rtype);
    try testing.expectEqual(@as(?u64, 0x1111), ns.holder);
    try testing.expectEqual(nvme.OP_RESV_REPORT, ns.last.opcode);
    try testing.expectEqual(@as(u64, 1), stats.pr_registrants_per_namespace.load());
    // The remote joiner.
    var rh: fake.Host = .{ .ns = &ns, .host_id = host_r };
    var r = try pr.Client.init(gpa, rh.face(), 1, 0x2222);
    defer r.deinit();
    _ = try r.probeRescap();
    const joined = try joinAsRegistrant(&r, null);
    try testing.expectEqual(@as(u16, 2), joined.regctl);
    try testing.expectEqual(@as(u16, 0), joined.cap_in_force);
    try testing.expect(!joined.shared);
    try testing.expect(ns.writeAllowed(host_r));
    try testing.expectEqual(@as(?u64, 0x1111), ns.holder);
    // The co-located joiner: no command mutates the device.
    const commands_before = ns.commands;
    const gen_before = ns.generation;
    var c = try pr.Client.init(gpa, mh.face(), 1, 0x3333);
    defer c.deinit();
    const adoptions0 = stats.pr_colocated_adoptions.load();
    try testing.expectEqual(@as(u64, 0x1111), try adoptColocated(&c, &.{ 0x9, 0x1111 }));
    try testing.expectEqual(gen_before, ns.generation);
    try testing.expect(ns.commands > commands_before);
    try testing.expectEqual(adoptions0 + 1, stats.pr_colocated_adoptions.load());
    try testing.expectError(error.HolderNotEnrolled, adoptColocated(&c, &.{0x9}));
    // A flat holder (rtype 1) refuses a registrant join.
    var flat = fake.Namespace.init(gpa);
    defer flat.deinit();
    try flat.seedHolder(0x77);
    var fh: fake.Host = .{ .ns = &flat, .host_id = host_r };
    var f = try pr.Client.init(gpa, fh.face(), 1, 0x2222);
    defer f.deinit();
    try testing.expectError(error.NotWero, joinAsRegistrant(&f, null));
    // No reservation at all: a co-located adopt finds no holder.
    var empty = fake.Namespace.init(gpa);
    defer empty.deinit();
    var eh: fake.Host = .{ .ns = &empty, .host_id = host_m };
    var e = try pr.Client.init(gpa, eh.face(), 1, 0x1);
    defer e.deinit();
    try testing.expectError(error.NoHolder, adoptColocated(&e, &.{0x1}));
}

test "the cap gate: a declared cap refuses the join at or past it loud (naming the remedy); the cap is LEARNED only from a repeated device-answered command-specific status at the same regctl and unlearned on a later success" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var mh: fake.Host = .{ .ns = &ns, .host_id = host_m };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    try acquireWero(&m);
    var rh: fake.Host = .{ .ns = &ns, .host_id = host_r };
    var r = try pr.Client.init(gpa, rh.face(), 1, 0x2222);
    defer r.deinit();
    _ = try joinAsRegistrant(&r, null);
    // Declared cap 2: the third registrant (regctl 3 at the gate) refuses
    // and leaves no residue.
    var sh: fake.Host = .{ .ns = &ns, .host_id = host_s };
    var s = try pr.Client.init(gpa, sh.face(), 1, 0x3333);
    defer s.deinit();
    const refusals0 = stats.pr_registrant_cap_refusals.load();
    try testing.expectError(error.RegistrantCap, joinAsRegistrant(&s, 2));
    try testing.expectEqual(refusals0 + 1, stats.pr_registrant_cap_refusals.load());
    try testing.expectEqual(@as(u64, 2), stats.pr_registrant_cap.load());
    try testing.expectEqual(@as(usize, 2), ns.registered.items.len);
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeCapRefusal(&w, "/dev/ng0n1", 2, 2);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "nvmet target") != null);
    try testing.expectError(error.RegistrantCap, registrantCapGate(8, 8));
    try registrantCapGate(7, 8);
    try registrantCapGate(1 << 15, 0);
    // The vendor cap: the device answers a command-specific status; one
    // refusal is a candidate, the repeat at the same regctl learns it.
    ns.registrant_cap = 2;
    try testing.expectError(error.RegistrantCap, joinAsRegistrant(&s, null));
    try testing.expectEqual(fake.CAP_STATUS, s.last_status);
    try testing.expectEqual(@as(?u16, null), s.cap.learned);
    try testing.expectEqual(@as(?u16, 2), s.cap.candidate);
    try testing.expectError(error.RegistrantCap, joinAsRegistrant(&s, null));
    try testing.expectEqual(@as(?u16, 2), s.cap.learned);
    try testing.expectEqual(@as(u16, 2), s.cap.effective(null));
    try testing.expectEqual(@as(u64, 2), stats.pr_registrant_cap.load());
    // The array grows (the cap lifted): a register success at or past the
    // learned count UNLEARNS it.
    ns.registrant_cap = null;
    const joined = try joinAsRegistrant(&s, null);
    try testing.expectEqual(@as(u16, 3), joined.regctl);
    try testing.expectEqual(@as(?u16, null), s.cap.learned);
    try testing.expectEqual(@as(u16, 0), s.cap.effective(null));
}

test "the death preempt: a usurper's key is preempted-and-aborted (fenced; the victim's writes refused at the device); the own-key guard withholds a co-located key; key 0 and no device take the tail scan; a device refusal is failed" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var mh: fake.Host = .{ .ns = &ns, .host_id = host_m };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    try acquireWero(&m);
    var rh: fake.Host = .{ .ns = &ns, .host_id = host_r };
    var r = try pr.Client.init(gpa, rh.face(), 1, 0x2222);
    defer r.deinit();
    _ = try joinAsRegistrant(&r, null);
    try testing.expect(ns.writeAllowed(host_r));
    const preempts0 = stats.appender_recovery_preempts.load();
    try testing.expectEqual(PreemptOutcome.fenced, preemptDeadRegistrant(&m, 0x2222));
    try testing.expectEqual(preempts0 + 1, stats.appender_recovery_preempts.load());
    try testing.expect(!ns.writeAllowed(host_r));
    try testing.expectEqual(nvme.RACQA_PREEMPT_AND_ABORT, ns.last.cdw10 & 0x7);
    try testing.expectEqual(@as(?u64, 0x1111), ns.holder);
    const guards0 = stats.appender_recovery_own_key_guards.load();
    try testing.expectEqual(PreemptOutcome.own_key_guard, preemptDeadRegistrant(&m, 0x1111));
    try testing.expectEqual(guards0 + 1, stats.appender_recovery_own_key_guards.load());
    try testing.expectEqual(PreemptOutcome.no_key, preemptDeadRegistrant(&m, 0));
    try testing.expectEqual(PreemptOutcome.no_device, preemptDeadRegistrant(null, 0x5));
    // An unknown victim: the device answers Invalid Field — failed, logged.
    try testing.expectEqual(PreemptOutcome.failed, preemptDeadRegistrant(&m, 0x4444));
}

const RamTerm = struct {
    term: ?u64 = null,
    stores: u32 = 0,
    fn load(ctx: ?*anyopaque) error{Io}!?u64 {
        const self: *RamTerm = @ptrCast(@alignCast(ctx.?));
        return self.term;
    }
    fn store(ctx: ?*anyopaque, t: u64) error{Io}!void {
        const self: *RamTerm = @ptrCast(@alignCast(ctx.?));
        self.term = t;
        self.stores += 1;
    }
    fn seam(self: *RamTerm) TermStore {
        return .{ .ctx = self, .load = &load, .store = &store };
    }
};

test "writer_term: the mount gate's arm is the recorded era + 1 (1 on a fresh volume), durable before the open proceeds, never lowered across a mount/unmount cycle; the token packs (term << 40) | seq" {
    var rt: RamTerm = .{};
    const a = try WriterTerm.arm(rt.seam());
    try testing.expectEqual(@as(u64, 1), a.term);
    try testing.expectEqual(@as(u32, 1), rt.stores);
    const b = try WriterTerm.arm(rt.seam());
    try testing.expectEqual(@as(u64, 2), b.term);
    try testing.expectEqual(@as(u64, 2), stats.writer_term.load());
    try testing.expectEqual(@as(u64, (2 << 40) | 77), b.token(77));
    try testing.expectEqual(@as(u64, 2), tokenTerm(b.token(77)));
    try testing.expect(tokenTerm(fencingToken(3, 1)) > tokenTerm(fencingToken(2, (1 << 40) - 1)));
}

test "the screen source: an unleased slot reads rule 4 inert; a leased one names its lessee at g; the Recovering suspension nulls the lessee while rules 1–3 stay armed; resume restores it; pr_fenced rides through" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    var src: ScreenSource = .{ .gpa = gpa, .table = t, .pr_fenced = true };
    defer src.deinit();
    var s = src.screenFor(5, null);
    try testing.expectEqual(@as(?u32, null), s.appender_current);
    try testing.expect(s.pr_fenced);
    _ = try t.acquire(5, 7, false);
    s = src.screenFor(5, .{ .g = 0, .tail = 4096 });
    try testing.expectEqual(@as(?u32, 7), s.appender_current);
    try testing.expectEqual(@as(u32, 1), s.g_current);
    try testing.expectEqual(@as(?u8, 4), s.foreignRule(.{ .appender_id = 0, .g = 1 }, 0, null));
    try testing.expectEqual(@as(?u8, 1), s.foreignRule(.{ .appender_id = 7, .g = 2 }, 0, null));
    try src.suspendLessee(7);
    try src.suspendLessee(7);
    try testing.expectEqual(@as(u64, 1), stats.fence_recovering_suspensions.load());
    s = src.screenFor(5, .{ .g = 0, .tail = 4096 });
    try testing.expectEqual(@as(?u32, null), s.appender_current);
    try testing.expectEqual(@as(?u8, null), s.foreignRule(.{ .appender_id = 0, .g = 1 }, 0, null));
    try testing.expectEqual(@as(?u8, 1), s.foreignRule(.{ .appender_id = 0, .g = 2 }, 0, null));
    try testing.expectEqual(@as(?u8, 2), s.foreignRule(.{ .appender_id = 7, .g = 0 }, 4096, null));
    try testing.expectEqual(@as(?u8, 3), s.foreignRule(.{ .appender_id = 7, .g = 0 }, 0, 1));
    src.resumeLessee(7);
    try testing.expectEqual(@as(u64, 0), stats.fence_recovering_suspensions.load());
    try testing.expectEqual(@as(?u32, 7), src.screenFor(5, null).appender_current);
}

test "the priming law: a stamp asked of an own leased slot before the prime is the (0, 0) tripwire; the prime stamps a re-adopted lease at g and a native slot at g + 1; a learned grant stamps; a slot leased to another appender is never ours" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    var fs = FrameStamps.init(gpa, 1);
    defer fs.deinit();
    _ = try t.acquire(3, 1, false);
    _ = try t.acquire(4, 2, false);
    const tw0 = stats.appender_unstamped_frame.load();
    stats.appender_unstamped_frame.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(FrameStamp{}, fs.stampFor(3, t));
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 1, stats.appender_unstamped_frame.load());
    try fs.prime(t, &.{ 3, 4 }, &.{9});
    try testing.expectEqual(FrameStamp{ .appender_id = 1, .g = 1 }, fs.stampFor(3, t));
    try testing.expectEqual(FrameStamp{ .appender_id = 1, .g = 1 }, fs.stampFor(9, t));
    // Another appender's slot: unstamped, and no tripwire (not ours).
    try testing.expectEqual(FrameStamp{}, fs.stampFor(4, t));
    try testing.expectEqual(tw0 + 1, stats.appender_unstamped_frame.load());
    try fs.learn(12, 5);
    try testing.expectEqual(FrameStamp{ .appender_id = 1, .g = 5 }, fs.stampFor(12, t));
    fs.forget(12);
    try testing.expectEqual(FrameStamp{}, fs.stampFor(12, t));
}
