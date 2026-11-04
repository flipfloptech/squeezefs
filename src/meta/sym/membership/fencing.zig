//! Fencing under the symmetric plane (charter §5.5 "Fencing"; `docs/
//! porting/sym_wire_membership_recovery.md` §4; decision 0003 §DF, §DG): the
//! metadata namespace holds WERO (rtype 3) by the MANAGER — the device keeps
//! ONE holder, so a successor's acquire conflicts and preempts it — with
//! every member HOST a REGISTRANT under its host's key; the ledger's preempt
//! of that key fences the host's members everywhere. It is issued by the
//! manager's FENCING REGISTRANT (`guard/fencer.zig`: a second host identity
//! of the manager's host under the same key, a non-holder) — nvmet 7.3
//! answers a HOLDER's preempt of another key with success and keeps the key,
//! a non-holder's takes the spec's path; with no fencer standing the holder's
//! own preempt is the posture (the report proves what landed: a kept key is
//! `.ignored`, the full tail scan — never a release, never re-fenced). A joiner NEVER acquires: one whose host is registered
//! under the key ADOPTS with ZERO device mutation (a register here destroyed
//! the fence in the rung-9 finding: `rtype 0, regctl 0`); another registers
//! and walks the CAP GATE — the report
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
//! slot's current `(g, lessee)` from the lease table, the recorded tail
//! of the leaf (tree 0's `slot_tails` as the table indexes them) and the
//! Recovering SUSPENSION — `appender_current = null` for a dead lessee's
//! slots from the recovery's step 2 to its step 8 (rule 4 inert, rules
//! 1–3 armed) — and the STAMP every frame carries, `(lessee, g)` off the
//! same table. `FramePlane` is the node cache's ONE seam for both halves
//! (`node_cache.FrameSeam`), installed by the slot plane at its PRIME.
//! The `(0, 0)` PRIMING law: every writer's open primes its frame stamps
//! at its FIRST frame writer (the claim gate's ring-recovery preflight
//! too); a stamp asked before the prime is the tripwire
//! `appender_unstamped_frame` (a `(0, 0)` frame on a leased leaf ENDS THE
//! LOG at the next load — an acked-writes loss class found twice).
//! Co-located mounts sharing one registrant are their own guarantee row:
//! `flock` + `T_self` + the frame screen; the recovery's own-key guard and
//! adopt-not-register are its two faces.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const knobs = @import("../../../core/knobs.zig");
const log = @import("../../../core/log.zig");
const pr = @import("../../guard/pr.zig");
const nvme = @import("../../guard/nvme_cmd.zig");
const bset = @import("../../bset.zig");
const key = @import("../../key.zig");
const node_cache = @import("../../node_cache.zig");
const route_table = @import("../../routing/route_table.zig");
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
    pub var pr_manager_holds: counters.ShardedCounter = .{};
    pub var pr_registrant_joins: counters.ShardedCounter = .{};
    pub var pr_colocated_adoptions: counters.ShardedCounter = .{};
    /// The death preempt landed (meta + data namespaces).
    pub var appender_recovery_preempts: counters.ShardedCounter = .{};
    /// The fencing registrant registered under the host's key (a manager's
    /// hold, a lapse's re-acquire).
    pub var pr_fencer_registrations: counters.ShardedCounter = .{};
    /// Death preempts that landed through the fencing registrant.
    pub var pr_fencer_preempts: counters.ShardedCounter = .{};
    /// A symmetric manager's hold with no fencing registrant standing (the
    /// holder's-preempt posture; the guard's `.stats` names why).
    pub var pr_fencer_unavailable: counters.ShardedCounter = .{};
    /// Death preempts withheld because a report did not show THIS host
    /// holding (a superseded manager finishing a recovery — review M1 of
    /// §DG); and fencer registrations taken back for the same reason.
    pub var pr_preempts_unheld_refused: counters.ShardedCounter = .{};
    pub var pr_fencer_unheld_unregistered: counters.ShardedCounter = .{};
    /// A preempt withheld because the dead key is THIS host's registrant.
    pub var appender_recovery_own_key_guards: counters.ShardedCounter = .{};
    /// The dead key found registered again after the page went Recovering
    /// (review B2): preempted again.
    pub var appender_recovery_refences: counters.ShardedCounter = .{};
    /// A preempt the target answered with success yet kept the key (nvmet
    /// 7.3's holder preempt): the dead host NOT fenced, the tail scan.
    pub var appender_recovery_preempts_ignored: counters.ShardedCounter = .{};
    /// The per-volume era in force.
    pub var writer_term: counters.Gauge = .{};
    /// Slots whose lessee's page is `Recovering` (rule 4 suspended).
    pub var fence_recovering_suspensions: counters.Gauge = .{};
    /// A `(0, 0)` stamp asked of a leased slot before the prime: must
    /// stay 0.
    pub var appender_unstamped_frame: counters.Tripwire = .init("appender_unstamped_frame");
    /// DATA namespaces (decision 0003 §DJ): a dead host's key preempted and
    /// proven gone (one per namespace).
    pub var data_pr_preempts: counters.ShardedCounter = .{};
    /// A data fence that could not be proven (a namespace's report or
    /// preempt failed, the key still registered, the namespace unreserved):
    /// the recovery holds, the next poll re-runs.
    pub var data_pr_fence_failures: counters.ShardedCounter = .{};
    /// A data fence withheld because a report did not show THIS host
    /// holding the metadata namespace (§DG.14's check on data).
    pub var data_pr_fence_unheld: counters.ShardedCounter = .{};
    /// A co-located member's death (the own-key guard) fenced by its
    /// recoverer on this host: the member's process proven dead or killed,
    /// every data node drained.
    pub var data_pr_colocated_kills: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// The manager's hold and the members' registrations.
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

/// The manager's hold: WERO under our key after the register ladder — the
/// device's one holder (decision 0003 §DF).
pub fn acquireManagerHold(client: *pr.Client) pr.Error!void {
    client.rtype = nvme.RTYPE_WERO;
    _ = try client.registerLadder();
    try client.acquire();
    stats.pr_manager_holds.inc();
    // The gauges read the namespace at every gate: the report after the hold.
    var rep = try client.report();
    rep.deinit(client.gpa);
    noteReport(client);
}

fn noteReport(client: *const pr.Client) void {
    stats.pr_registrants_per_namespace.set(client.regctl);
    stats.pr_report_bytes.set(client.report_bytes);
}

/// `SQUEEZEFS_PR_REGISTRANT_CAP`: a DECLARED cap for a vendor array
/// (charter §5.5); null = learn (unbounded on nvmet by construction).
pub fn declaredRegistrantCap(env: *const std.process.Environ.Map) ?u16 {
    return knobs.optIntKnob("SQUEEZEFS_PR_REGISTRANT_CAP", u16, env);
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

/// A joiner host: register under ONE key, read the report, feed the
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
    if (rep.rtype != 0 and rep.rtype != nvme.RTYPE_WERO) {
        client.unregister(client.key) catch {};
        return error.NotWero;
    }
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

/// A CO-LOCATED joiner: ZERO device mutation — the standing hold read and
/// one of the enrolled keys (the host's) found holding under it. Answers
/// that key.
pub fn adoptColocated(client: *pr.Client, enrolled: []const u64) JoinError!u64 {
    var rep = client.report() catch return error.Device;
    defer rep.deinit(client.gpa);
    noteReport(client);
    if (rep.rtype == 0) return error.NoHolder;
    if (rep.rtype != nvme.RTYPE_WERO) return error.NotWero;
    for (enrolled) |k| if (rep.heldBy(k)) {
        stats.pr_colocated_adoptions.inc();
        return k;
    };
    return error.HolderNotEnrolled;
}

pub const PreemptOutcome = enum {
    /// The preempt-and-abort LANDED: `pr_fenced`, the tail scan skipped.
    fenced,
    /// The target answered success and KEPT the key (nvmet 7.3: the
    /// holder's preempt only re-types its own hold): nothing fenced, the
    /// full tail scan; no re-fence (the key never left — a re-read would
    /// count it "registered again" and re-issue the same no-op).
    ignored,
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
/// namespace `client` holds (`RACQA 2`; nvmet runs it as a plain preempt),
/// issued by the fencing registrant when one stands (a non-holder under
/// `client`'s key, decision 0003 §DG), else by the holder. `pr_fenced ≡
/// fenced`.
pub fn preemptDeadRegistrant(client: ?*pr.Client, fencer: ?*pr.Client, victim_key: u64) PreemptOutcome {
    const c = client orelse return .no_device;
    if (victim_key == 0) return .no_key;
    // Our own key: a preempt of it unregisters the holder — unreserved —
    // and the fencer with it (nvmet does not skip the sender).
    if (victim_key == c.key) {
        stats.appender_recovery_own_key_guards.inc();
        return .own_key_guard;
    }
    // Only while THIS host holds (our key under our host id; review M1 of
    // §DG): a fencer registered after a successor preempted our key would
    // otherwise fence the successor's members, and take the reservation
    // when the victim's key is the holder's. Under the fencer's lock — the
    // heartbeat's re-register and its check take it too — so the fencer's
    // registration changes only by a preempt between this read and the
    // preempt: a takeover, which removes it with our host (one key) and
    // makes its command conflict.
    // The holder's sequence lock first, then the fencer's (the heartbeat's
    // order): the holder client's state has one writer at a time (review
    // minor 4).
    c.lockOps();
    defer c.unlockOps();
    if (fencer) |f| f.lockOps();
    defer if (fencer) |f| f.unlockOps();
    const ours = c.holdsHere() catch |e| {
        log.warn("recovery preempt: the report before the preempt of key 0x{x} failed ({s}) — nothing preempted; the full tail scan is the fence", .{ victim_key, @errorName(e) });
        return .failed;
    };
    if (!ours) {
        stats.pr_preempts_unheld_refused.inc();
        log.warn("recovery preempt: the reservation is not held by this host — the preempt of key 0x{x} is withheld (a successor took the hold: this manager is superseded); the full tail scan is the fence", .{victim_key});
        return .failed;
    }
    const issuer = fencer orelse c;
    issuer.preemptAndAbort(victim_key) catch |e| return preemptFailed(issuer, victim_key, e);
    // The report proves it: a target that answered success yet kept the
    // key (nvmet 7.3: its HOLDER's preempt only re-types its own hold)
    // fenced nothing — the full tail scan, never a release (that would leave
    // the namespace unreserved). A report that fails proves nothing fenced:
    // the full tail scan too (review 4 minor 3).
    const kept = keyRegistered(c, victim_key) catch {
        log.warn("recovery preempt: the report after the preempt of key 0x{x} failed — nothing proven fenced; the full tail scan is the fence", .{victim_key});
        return .failed;
    };
    if (kept) {
        stats.appender_recovery_preempts_ignored.inc();
        log.warn("recovery preempt: the target answered the preempt of key 0x{x} ({s}) yet keeps it registered — the dead member's host is NOT fenced; the full tail scan is the fence", .{ victim_key, if (fencer != null) "by the fencing registrant" else "by the holder: nvmet 7.3 only re-types its own hold" });
        return .ignored;
    }
    stats.appender_recovery_preempts.inc();
    if (fencer != null) stats.pr_fencer_preempts.inc();
    return .fenced;
}

/// Review B2's recovery side: once the page is Recovering (step 2), the
/// report read again — a registration of the dead member's host that raced
/// step 1 (its preempt found the key absent: an earlier death had fenced the
/// host, and a sibling's rule registered it meanwhile) is preempted again
/// before the ring is read. Null = the key stands absent (or no device, or
/// no key); a report that fails is the full tail scan's.
pub fn refenceAfterRecovering(client: ?*pr.Client, fencer: ?*pr.Client, victim_key: u64) ?PreemptOutcome {
    const c = client orelse return null;
    if (victim_key == 0 or victim_key == c.key) return null;
    c.lockOps();
    const back = keyRegistered(c, victim_key) catch {
        c.unlockOps();
        return .failed;
    };
    c.unlockOps();
    if (!back) return null;
    stats.appender_recovery_refences.inc();
    log.warn("recovery preempt: key 0x{x} is registered again once the page is Recovering (the dead member's host registered meanwhile) — preempted again", .{victim_key});
    return preemptDeadRegistrant(c, fencer, victim_key);
}

fn preemptFailed(c: *pr.Client, victim_key: u64, e: pr.Error) PreemptOutcome {
    var buf: [128]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    c.writeError(&w, e) catch {};
    log.warn("recovery preempt: preempt-and-abort of key 0x{x} did not land ({s}) — the full tail scan is the fence", .{ victim_key, w.buffered() });
    return .failed;
}

/// Recovery's data fence (decision 0003 §DJ; step 2c, after the page went
/// Recovering): every DATA namespace — reserved Write Exclusive – All
/// Registrants, so ANY registrant's preempt of another key unregisters every
/// registrant of it and keeps the reservation (nvmet 7.3's `nvmet_pr_preempt`
/// runs its all-registrants arm before the single-holder re-type: the
/// manager's own client fences, no fencer) — loses the dead host's key, and
/// the report proves it: the namespace reserved WEAR and no registrant under
/// the key. Issued only while a report shows THIS host holding the metadata
/// namespace `meta` (§DG.14), under its sequence lock. `.failed` / `.unheld`
/// hold the recovery: the dead member's extents are reallocatable only once
/// every namespace that can hold them fences its host — data carries no
/// frame screen to fall back on.
pub const DataFenceOutcome = enum {
    /// Every data namespace proven: the key absent, the reservation standing.
    fenced,
    /// No data namespace carries PR (the documented no-device-fence class).
    no_device,
    /// A key-less record (`appender clear`, an EARLY same-node death): the
    /// member is proven dead otherwise.
    no_key,
    /// The dead key is this host's (a co-located member): the device cannot
    /// fence inside one host — the caller's kill on its own host.
    own_key_guard,
    /// This host does not hold the metadata namespace: a superseded manager
    /// fences nobody.
    unheld,
    /// A namespace's fence is not proven.
    failed,
};

pub fn fenceDataNamespaces(meta: ?*pr.Client, data: []const *pr.Client, victim_key: u64) DataFenceOutcome {
    var unproven: usize = 0;
    return fenceDataNamespacesCounted(meta, data, victim_key, &unproven);
}

/// The same; `unproven` = the namespaces whose fence is not proven (all of
/// them when nothing was issued: the hold check failed).
pub fn fenceDataNamespacesCounted(meta: ?*pr.Client, data: []const *pr.Client, victim_key: u64, unproven: *usize) DataFenceOutcome {
    unproven.* = 0;
    if (data.len == 0) return .no_device;
    const m = meta orelse return .no_device;
    if (victim_key == 0) return .no_key;
    // Every data client registers under this host's key (the metadata
    // preempt counted the own-key guard already).
    if (victim_key == data[0].key) return .own_key_guard;
    m.lockOps();
    defer m.unlockOps();
    const held = m.holdsHere() catch |e| {
        stats.data_pr_fence_failures.inc();
        log.warn("recovery data fence: the metadata report before the preempt of key 0x{x} failed ({s}) — nothing preempted; the recovery holds", .{ victim_key, @errorName(e) });
        unproven.* = data.len;
        return .failed;
    };
    if (!held) {
        stats.data_pr_fence_unheld.inc();
        log.warn("recovery data fence: the metadata reservation is not held by this host — the data preempt of key 0x{x} is withheld (a successor took the hold: this manager is superseded)", .{victim_key});
        return .unheld;
    }
    for (data) |c| {
        if (!fenceDataNamespace(c, victim_key)) unproven.* += 1;
    }
    return if (unproven.* == 0) .fenced else .failed;
}

/// One data namespace: the key registered ⇒ preempt-and-abort (RACQA 2: the
/// target drains the victim's admitted writes before it completes); then the
/// report: reserved WEAR, the key absent. True = proven.
fn fenceDataNamespace(c: *pr.Client, victim_key: u64) bool {
    const before = keyRegistered(c, victim_key) catch |e| return dataFenceFailed(c, victim_key, "the report", e);
    if (before) c.preemptAndAbort(victim_key) catch |e| {
        var buf: [128]u8 = undefined;
        var w: std.Io.Writer = .fixed(&buf);
        c.writeError(&w, e) catch {};
        log.warn("recovery data fence: the preempt-and-abort of key 0x{x} on data nsid {d} did not land ({s}) — the report decides", .{ victim_key, c.nsid, w.buffered() });
    };
    var rep = c.report() catch |e| return dataFenceFailed(c, victim_key, "the report after the preempt", e);
    defer rep.deinit(c.gpa);
    var kept = false;
    for (rep.registrants) |r| kept = kept or r.rkey == victim_key;
    if (kept or rep.rtype != nvme.RTYPE_WEAR) {
        stats.data_pr_fence_failures.inc();
        log.warn("recovery data fence: data nsid {d} {s} after the preempt of key 0x{x} (rtype {d}) — the dead member's host is NOT fenced there; the recovery holds", .{ c.nsid, if (kept) "keeps the key registered" else "carries no WEAR reservation", victim_key, rep.rtype });
        return false;
    }
    if (before) stats.data_pr_preempts.inc();
    return true;
}

fn dataFenceFailed(c: *pr.Client, victim_key: u64, what: []const u8, e: pr.Error) bool {
    var buf: [128]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    c.writeError(&w, e) catch {};
    stats.data_pr_fence_failures.inc();
    log.warn("recovery data fence: {s} of data nsid {d} for key 0x{x} failed ({s}) — the fence is not proven; the recovery holds", .{ what, c.nsid, victim_key, w.buffered() });
    return false;
}

/// Does a registrant hold `key` on the namespace (the report)?
fn keyRegistered(c: *pr.Client, rkey: u64) pr.Error!bool {
    var rep = try c.report();
    defer rep.deinit(c.gpa);
    for (rep.registrants) |r| if (r.rkey == rkey) return true;
    return false;
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

    /// The stamp a frame on `slot`'s leaf carries: a stamp asked BEFORE
    /// the prime is the `(0, 0)` tripwire — the frame is written unstamped
    /// exactly as the reference's bug did, counted and loud, never a
    /// panic; after it, the learned `g` (the prime's words, a grant), else
    /// the table's `g` for a slot this writer leases, else — the manager's
    /// STRUCTURAL class — `(0, g)` on an unleased slot (the maintaining
    /// manager's frames beside the former lessee's) and on a dead lessee's
    /// slot mid-recovery (the recoverer's frames are the recovery's own);
    /// a slot another appender leases is never ours to stamp.
    pub fn stampFor(self: *const FrameStamps, slot: u16, table: *const LeaseTable) FrameStamp {
        if (!self.primed) {
            stats.appender_unstamped_frame.fire("a frame asked for its stamp before the writer's prime (a (0, 0) frame on a leased leaf ends the log at the next load)");
            return .{};
        }
        if (self.stamps.get(slot)) |g| return .{ .appender_id = self.me, .g = g };
        const v = table.view(slot);
        if (v.leasedBy(self.me) and !v.dead) return .{ .appender_id = self.me, .g = v.g };
        if (self.me == lease_table.MANAGER and (v.state == .unleased or v.state == .migrating or v.dead)) return .{ .appender_id = lease_table.MANAGER, .g = v.g };
        return .{};
    }
};

// ---------------------------------------------------------------------------
// The frame plane: the node cache's ONE source for both halves.
// ---------------------------------------------------------------------------

/// Test seam: the slot plane's prime SKIPPED at `init` (the writer stays
/// unprimed until its arm) — the reference's twice-found loss class, held
/// open so the red test pins what the priming law prevents. One acquire
/// load per open when unarmed.
pub var test_unprimed_first_writer: std.atomic.Value(bool) = .init(false);

/// The node cache's `FrameSeam` over the lease table (installed at the
/// slot plane's prime, BEFORE the open's first frame writer): the screen
/// for a slot-tree load — the slot's `(g, lessee)`, the recorded tail of
/// the leaf (tree 0's `slot_tails` as the table indexes them), the
/// Recovering suspension — and the stamp a frame carries: an in-process
/// region's lease stamps `(holder, g)`; everything else (the manager's own
/// leases, the unleased trees, a dead lessee's mid-recovery) is the
/// manager's `FrameStamps` — whose prime overrides carry the arm's words
/// until the arm lands and the table alone rules.
pub const FramePlane = struct {
    gpa: Allocator,
    table: *LeaseTable,
    /// The routing slot the native forest slot names on this volume.
    native_slot: ?u16,
    screen: ScreenSource,
    stamps: FrameStamps,

    pub fn init(gpa: Allocator, table: *LeaseTable, native_slot: ?u16) FramePlane {
        return .{ .gpa = gpa, .table = table, .native_slot = native_slot, .screen = .{ .gpa = gpa, .table = table, .pr_fenced = false }, .stamps = FrameStamps.init(gpa, lease_table.MANAGER) };
    }

    pub fn deinit(self: *FramePlane) void {
        self.screen.deinit();
        self.stamps.deinit();
    }

    /// The device fence in force (rule 1/4's class): known at the gate,
    /// after the prime.
    pub fn setFenced(self: *FramePlane, fenced: bool) void {
        self.screen.pr_fenced = fenced;
    }

    /// `prime_frame_stamps` (§4.5): tree 0's population is in the table
    /// (the caller's load); `held` = the slots it leases to the manager,
    /// `native` = the arm's own slots unleased in tree 0 (stamped `g + 1`).
    pub fn prime(self: *FramePlane, held: []const u16, native: []const u16) Allocator.Error!void {
        try self.stamps.prime(self.table, held, native);
    }

    pub fn primed(self: *const FramePlane) bool {
        return self.stamps.primed;
    }

    /// The arm landed: every lease the prime anticipated is the table's
    /// — the overrides retire (a stale override would outlive a handover);
    /// a writer the seam left unprimed is primed late here (the arm loaded
    /// the population).
    pub fn noteArmed(self: *FramePlane) void {
        self.stamps.stamps.clearRetainingCapacity();
        self.stamps.primed = true;
    }

    pub fn seam(self: *FramePlane) node_cache.FrameSeam {
        return .{ .ctx = self, .screenFor = &screenForFn, .stampFor = &stampForFn };
    }

    /// The routing slot of a forest slot on this volume.
    pub fn routingSlotOf(self: *const FramePlane, fs: key.ForestSlot) u16 {
        if (fs == key.NATIVE_FOREST_SLOT) return self.native_slot orelse 0;
        return route_table.slotOfForestSlot(fs);
    }

    pub fn screenForSlot(self: *FramePlane, slot: u16, leaf_addr: u64) FrameScreen {
        const t = self.table.recordedTail(slot, leaf_addr);
        return self.screen.screenFor(slot, if (t) |r| .{ .g = r.g, .tail = r.tail } else null);
    }

    pub fn stampForSlot(self: *const FramePlane, slot: u16) FrameStamp {
        const v = self.table.view(slot);
        if (self.stamps.primed and v.state != .unleased and v.state != .migrating and !v.dead and v.holder != lease_table.MANAGER) return .{ .appender_id = v.holder, .g = v.g };
        return self.stamps.stampFor(slot, self.table);
    }

    fn screenForFn(ctx: *anyopaque, fs: key.ForestSlot, addr: u64) ?FrameScreen {
        const self: *FramePlane = @ptrCast(@alignCast(ctx));
        return self.screenForSlot(self.routingSlotOf(fs), addr);
    }

    fn stampForFn(ctx: *anyopaque, fs: key.ForestSlot) FrameStamp {
        const self: *FramePlane = @ptrCast(@alignCast(ctx));
        return self.stampForSlot(self.routingSlotOf(fs));
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

test "the manager's hold is WERO (rtype 3 on the wire, one holder); a joiner host registers under ONE key and reads the report sized by REGCTL; a co-located joiner adopts with zero device mutation, one of its enrolled keys found holding; a flat (rtype 1) holder refuses the join" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var mh: fake.Host = .{ .ns = &ns, .host_id = host_m };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    _ = try m.probeRescap();
    try acquireManagerHold(&m);
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
    try acquireManagerHold(&m);
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
    try acquireManagerHold(&m);
    var rh: fake.Host = .{ .ns = &ns, .host_id = host_r };
    var r = try pr.Client.init(gpa, rh.face(), 1, 0x2222);
    defer r.deinit();
    _ = try joinAsRegistrant(&r, null);
    try testing.expect(ns.writeAllowed(host_r));
    const preempts0 = stats.appender_recovery_preempts.load();
    try testing.expectEqual(PreemptOutcome.fenced, preemptDeadRegistrant(&m, null, 0x2222));
    try testing.expectEqual(preempts0 + 1, stats.appender_recovery_preempts.load());
    try testing.expect(!ns.writeAllowed(host_r));
    try testing.expectEqual(nvme.RACQA_PREEMPT_AND_ABORT, ns.last_acquire.cdw10 & 0x7);
    try testing.expectEqual(@as(?u64, 0x1111), ns.holder);
    const guards0 = stats.appender_recovery_own_key_guards.load();
    try testing.expectEqual(PreemptOutcome.own_key_guard, preemptDeadRegistrant(&m, null, 0x1111));
    try testing.expectEqual(guards0 + 1, stats.appender_recovery_own_key_guards.load());
    try testing.expectEqual(PreemptOutcome.no_key, preemptDeadRegistrant(&m, null, 0));
    try testing.expectEqual(PreemptOutcome.no_device, preemptDeadRegistrant(null, null, 0x5));
    // An unknown victim: the device answers Invalid Field — failed, logged.
    try testing.expectEqual(PreemptOutcome.failed, preemptDeadRegistrant(&m, null, 0x4444));
}

test "the death preempt under the manager's WERO hold: on nvmet 7.3's semantics the holder's preempt answers success yet keeps the victim registered — .ignored (the full tail scan, counted, no re-fence), never a release that would leave the namespace unreserved; on a spec-conformant target the same preempt fences the victim's host with the hold kept; the re-fence preempts a key registered again" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    ns.holder_preempt_retypes = true;
    var mh: fake.Host = .{ .ns = &ns, .host_id = host_m };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    try acquireManagerHold(&m);
    var rh: fake.Host = .{ .ns = &ns, .host_id = host_r };
    var r = try pr.Client.init(gpa, rh.face(), 1, 0x2222);
    defer r.deinit();
    _ = try joinAsRegistrant(&r, null);
    const ignored0 = stats.appender_recovery_preempts_ignored.load();
    try testing.expectEqual(PreemptOutcome.ignored, preemptDeadRegistrant(&m, null, 0x2222));
    try testing.expectEqual(ignored0 + 1, stats.appender_recovery_preempts_ignored.load());
    try testing.expect(ns.writeAllowed(host_r));
    try testing.expectEqual(@as(?u64, 0x1111), ns.holder);
    try testing.expectEqual(nvme.RTYPE_WERO, ns.rtype);
    // A spec-conformant target: the holder's preempt fences the victim.
    var ns2 = fake.Namespace.init(gpa);
    defer ns2.deinit();
    var mh2: fake.Host = .{ .ns = &ns2, .host_id = host_m };
    var m2 = try pr.Client.init(gpa, mh2.face(), 1, 0x1111);
    defer m2.deinit();
    try acquireManagerHold(&m2);
    var rh2: fake.Host = .{ .ns = &ns2, .host_id = host_r };
    var r2 = try pr.Client.init(gpa, rh2.face(), 1, 0x2222);
    defer r2.deinit();
    _ = try joinAsRegistrant(&r2, null);
    try testing.expectEqual(PreemptOutcome.fenced, preemptDeadRegistrant(&m2, null, 0x2222));
    try testing.expect(!ns2.writeAllowed(host_r));
    try testing.expect(ns2.writeAllowed(host_m));
    try testing.expectEqual(@as(?u64, 0x1111), ns2.holder);
    try testing.expectEqual(nvme.RTYPE_WERO, ns2.rtype);
    // The re-fence (review B2): the key registered again is preempted again.
    _ = try joinAsRegistrant(&r2, null);
    try testing.expectEqual(@as(?PreemptOutcome, .fenced), refenceAfterRecovering(&m2, null, 0x2222));
    try testing.expect(!ns2.writeAllowed(host_r));
    try testing.expectEqual(@as(?PreemptOutcome, null), refenceAfterRecovering(&m2, null, 0x2222));
    try testing.expectEqual(@as(?PreemptOutcome, null), refenceAfterRecovering(&m2, null, 0x1111));
    try testing.expectEqual(@as(?PreemptOutcome, null), refenceAfterRecovering(null, null, 0x2222));
}

const host_f: [16]u8 = "fencer-host-id-4".*;

test "the death preempt through the fencing registrant (decision 0003 §DG): on nvmet 7.3's semantics the fencer — a second host id under the holder's key, a NON-holder — unregisters the victim's host and keeps the reservation (fenced, counted on pr_fencer_preempts), where the holder's own preempt was ignored; the re-fence goes through it too; the own-key guard withholds our key (a preempt of it would unregister the holder and the fencer)" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    ns.holder_preempt_retypes = true;
    var mh: fake.Host = .{ .ns = &ns, .host_id = host_m };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    try acquireManagerHold(&m);
    var fh: fake.Host = .{ .ns = &ns, .host_id = host_f };
    var f = try pr.Client.init(gpa, fh.face(), 1, 0x1111);
    defer f.deinit();
    f.rtype = nvme.RTYPE_WERO;
    _ = try f.registerLadder();
    var rh: fake.Host = .{ .ns = &ns, .host_id = host_r };
    var r = try pr.Client.init(gpa, rh.face(), 1, 0x2222);
    defer r.deinit();
    _ = try joinAsRegistrant(&r, null);
    // The fencer is not the holder: the report names the host's registrant.
    try testing.expect(ns.isHolder(host_m));
    try testing.expect(!ns.isHolder(host_f));
    const preempts0 = stats.appender_recovery_preempts.load();
    const fencer0 = stats.pr_fencer_preempts.load();
    const ignored0 = stats.appender_recovery_preempts_ignored.load();
    try testing.expectEqual(PreemptOutcome.fenced, preemptDeadRegistrant(&m, &f, 0x2222));
    try testing.expectEqual(preempts0 + 1, stats.appender_recovery_preempts.load());
    try testing.expectEqual(fencer0 + 1, stats.pr_fencer_preempts.load());
    try testing.expectEqual(ignored0, stats.appender_recovery_preempts_ignored.load());
    try testing.expect(!ns.writeAllowed(host_r));
    try testing.expect(ns.isHolder(host_m));
    try testing.expectEqual(nvme.RTYPE_WERO, ns.rtype);
    try testing.expect(ns.find(host_f) != null);
    try testing.expectEqual(nvme.RACQA_PREEMPT_AND_ABORT, ns.last_acquire.cdw10 & 0x7);
    // The re-fence: the key registered again is preempted again — through
    // the fencer, so it lands on nvmet too.
    _ = try joinAsRegistrant(&r, null);
    try testing.expectEqual(@as(?PreemptOutcome, .fenced), refenceAfterRecovering(&m, &f, 0x2222));
    try testing.expect(!ns.writeAllowed(host_r));
    // Our own key: withheld — nothing issued.
    const commands0 = ns.commands;
    try testing.expectEqual(PreemptOutcome.own_key_guard, preemptDeadRegistrant(&m, &f, 0x1111));
    try testing.expectEqual(commands0, ns.commands);
    try testing.expect(ns.isHolder(host_m));
    try testing.expect(ns.find(host_f) != null);
    // A spec-conformant target: the same path, the same outcome — one path
    // for every target (decision 0003 §DG.6).
    var ns2 = fake.Namespace.init(gpa);
    defer ns2.deinit();
    var mh2: fake.Host = .{ .ns = &ns2, .host_id = host_m };
    var m2 = try pr.Client.init(gpa, mh2.face(), 1, 0x1111);
    defer m2.deinit();
    try acquireManagerHold(&m2);
    var fh2: fake.Host = .{ .ns = &ns2, .host_id = host_f };
    var f2 = try pr.Client.init(gpa, fh2.face(), 1, 0x1111);
    defer f2.deinit();
    f2.rtype = nvme.RTYPE_WERO;
    _ = try f2.registerLadder();
    var rh2: fake.Host = .{ .ns = &ns2, .host_id = host_r };
    var r2 = try pr.Client.init(gpa, rh2.face(), 1, 0x2222);
    defer r2.deinit();
    _ = try joinAsRegistrant(&r2, null);
    try testing.expectEqual(PreemptOutcome.fenced, preemptDeadRegistrant(&m2, &f2, 0x2222));
    try testing.expect(!ns2.writeAllowed(host_r));
    try testing.expect(ns2.isHolder(host_m));
    try testing.expect(ns2.find(host_f) != null);
}

test "the fencer's key is its host's (decision 0003 §DG.2): a successor's takeover preempt of the holder's key unregisters the host AND its fencer in one command — the superseded manager's fencer then answers a conflict (failed, nothing fenced); a fencer under a key of its own outlives the takeover and, preempting the new holder's key, TAKES the reservation" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    ns.holder_preempt_retypes = true;
    var mh: fake.Host = .{ .ns = &ns, .host_id = host_m };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    try acquireManagerHold(&m);
    var fh: fake.Host = .{ .ns = &ns, .host_id = host_f };
    var f = try pr.Client.init(gpa, fh.face(), 1, 0x1111);
    defer f.deinit();
    f.rtype = nvme.RTYPE_WERO;
    _ = try f.registerLadder();
    var sh: fake.Host = .{ .ns = &ns, .host_id = host_s };
    var s = try pr.Client.init(gpa, sh.face(), 1, 0x3333);
    defer s.deinit();
    s.rtype = nvme.RTYPE_WERO;
    _ = try s.registerLadder();
    try s.preempt(0x1111);
    try testing.expect(ns.isHolder(host_s));
    try testing.expect(ns.find(host_m) == null);
    try testing.expect(ns.find(host_f) == null);
    // The superseded manager, resumed mid-recovery: the report shows another
    // host holding — the preempt withheld (review M1); and the fencer's own
    // command conflicts at the device anyway (its registration went with the
    // host's) — nothing fenced, the successor's hold intact.
    const refused0 = stats.pr_preempts_unheld_refused.load();
    try testing.expectEqual(PreemptOutcome.failed, preemptDeadRegistrant(&m, &f, 0x3333));
    try testing.expectEqual(refused0 + 1, stats.pr_preempts_unheld_refused.load());
    try testing.expectError(error.ReservationConflict, f.preemptAndAbort(0x3333));
    try testing.expect(ns.isHolder(host_s));
    try testing.expect(ns.writeAllowed(host_s));
    // The weakening on the device: a fencer under a key of its own.
    var ns2 = fake.Namespace.init(gpa);
    defer ns2.deinit();
    ns2.holder_preempt_retypes = true;
    var mh2: fake.Host = .{ .ns = &ns2, .host_id = host_m };
    var m2 = try pr.Client.init(gpa, mh2.face(), 1, 0x1111);
    defer m2.deinit();
    try acquireManagerHold(&m2);
    var fh2: fake.Host = .{ .ns = &ns2, .host_id = host_f };
    var f2 = try pr.Client.init(gpa, fh2.face(), 1, 0xF00D);
    defer f2.deinit();
    f2.rtype = nvme.RTYPE_WERO;
    _ = try f2.registerLadder();
    var sh2: fake.Host = .{ .ns = &ns2, .host_id = host_s };
    var s2 = try pr.Client.init(gpa, sh2.face(), 1, 0x3333);
    defer s2.deinit();
    s2.rtype = nvme.RTYPE_WERO;
    _ = try s2.registerLadder();
    try s2.preempt(0x1111);
    try testing.expect(ns2.find(host_f) != null);
    try f2.preemptAndAbort(0x3333);
    try testing.expect(ns2.isHolder(host_f));
    try testing.expect(!ns2.writeAllowed(host_s));
}

test "review 4 minor 3: a report that fails after the death preempt fenced nothing proven — .failed (the full tail scan), never .fenced" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    ns.holder_preempt_retypes = true;
    var mh: fake.Host = .{ .ns = &ns, .host_id = host_m };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    try acquireManagerHold(&m);
    var rh: fake.Host = .{ .ns = &ns, .host_id = host_r };
    var r = try pr.Client.init(gpa, rh.face(), 1, 0x2222);
    defer r.deinit();
    _ = try joinAsRegistrant(&r, null);
    const preempts0 = stats.appender_recovery_preempts.load();
    // The host id read and cached; the report before the preempt (two
    // commands) and the preempt pass; the report after it fails.
    _ = try m.wireHostId();
    ns.fail_next = error.InputOutput;
    ns.fail_skip = 3;
    try testing.expectEqual(PreemptOutcome.failed, preemptDeadRegistrant(&m, null, 0x2222));
    // The preempt reached the device; the report after it failed.
    try testing.expectEqual(nvme.RACQA_PREEMPT_AND_ABORT, ns.last_acquire.cdw10 & 0x7);
    try testing.expectEqual(preempts0, stats.appender_recovery_preempts.load());
    try testing.expect(ns.writeAllowed(host_r));
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
    // A primed writer's lease the table names without a learned word
    // stamps the table's g; the MANAGER's structural class stamps (0, g)
    // on an unleased slot and on a dead lessee's slot mid-recovery.
    _ = try t.acquire(20, 1, false);
    try testing.expectEqual(FrameStamp{ .appender_id = 1, .g = 1 }, fs.stampFor(20, t));
    var mgr = FrameStamps.init(gpa, lease_table.MANAGER);
    defer mgr.deinit();
    try mgr.prime(t, &.{}, &.{});
    try testing.expectEqual(FrameStamp{ .appender_id = 0, .g = 0 }, mgr.stampFor(30, t));
    _ = try t.acquire(30, 5, false);
    try testing.expectEqual(FrameStamp{}, mgr.stampFor(30, t));
    _ = t.beginRelease(30, 5, true);
    try testing.expectEqual(FrameStamp{ .appender_id = 0, .g = 1 }, mgr.stampFor(30, t));
}

test "the frame plane: BOTH halves from the table — an in-process region's lease stamps (holder, g), the manager's own and the unleased trees (0, g), a dead lessee's slot (0, g); the prime's overrides (a re-adopted lease at g, the native slot at g + 1) stand until the arm; the screen reads the lessee, the recorded tail of the leaf and the Recovering suspension; a stamp asked unprimed is the (0, 0) tripwire" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    // Tree 0's population: slot 3 leased to the manager at g 2, slot 7 to
    // appender 1 at g 4, the native slot 0 released at g 1 with a tail.
    try t.load(3, .{ .variant = .leased, .appender_id = 0, .g = 2, .page_addr = 4096, .root = .{ .addr = 0x10000, .seq = 1 }, .cursor = 10, .slot_tree_extents = 1 });
    try t.load(7, .{ .variant = .leased, .appender_id = 1, .g = 4, .page_addr = 8192, .root = .{ .addr = 0x20000, .seq = 1 }, .cursor = 10, .slot_tree_extents = 1 });
    try t.load(0, .{ .variant = .unleased, .g = 1, .root = .{ .addr = 0x30000, .seq = 1 }, .cursor = 10, .slot_tree_extents = 1 });
    try t.recordTails(0, 1, &.{.{ .leaf_addr = 0x30000, .tail = 12288 }});
    var fp = FramePlane.init(gpa, t, 0);
    defer fp.deinit();
    const seam = fp.seam();
    // Unprimed: the tripwire, the structural (0, 0).
    const tw0 = stats.appender_unstamped_frame.load();
    stats.appender_unstamped_frame.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(FrameStamp{}, seam.stampFor(seam.ctx, key.guestForestSlot(3)));
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 1, stats.appender_unstamped_frame.load());
    // The prime: the manager's held slots at g, the native slot at g + 1.
    try fp.prime(&.{3}, &.{0});
    try testing.expect(fp.primed());
    try testing.expectEqual(FrameStamp{ .appender_id = 0, .g = 2 }, seam.stampFor(seam.ctx, key.guestForestSlot(3)));
    try testing.expectEqual(FrameStamp{ .appender_id = 0, .g = 2 }, seam.stampFor(seam.ctx, key.NATIVE_FOREST_SLOT));
    // An in-process region's lease: (holder, g) off the table.
    try testing.expectEqual(FrameStamp{ .appender_id = 1, .g = 4 }, seam.stampFor(seam.ctx, key.guestForestSlot(7)));
    // An unleased slot nobody recorded: the manager's (0, 0) at g 0.
    try testing.expectEqual(FrameStamp{ .appender_id = 0, .g = 0 }, seam.stampFor(seam.ctx, key.guestForestSlot(9)));
    // The arm lands the native lease at g + 1 (the override's word) and
    // retires the overrides: the table alone rules.
    _ = try t.acquire(0, 0, false);
    fp.noteArmed();
    try testing.expectEqual(@as(usize, 0), fp.stamps.stamps.count());
    try testing.expectEqual(FrameStamp{ .appender_id = 0, .g = 2 }, seam.stampFor(seam.ctx, key.NATIVE_FOREST_SLOT));
    try testing.expectEqual(FrameStamp{ .appender_id = 0, .g = 2 }, seam.stampFor(seam.ctx, key.guestForestSlot(3)));
    // A handover of slot 3 to appender 1 at g 3: the frames follow the lease.
    _ = t.beginRelease(3, 0, false);
    _ = try t.release(3, 0, 2, .{}, 0);
    _ = try t.acquire(3, 1, false);
    try testing.expectEqual(FrameStamp{ .appender_id = 1, .g = 3 }, seam.stampFor(seam.ctx, key.guestForestSlot(3)));
    // A dead lessee mid-recovery: the recoverer's structural (0, g).
    _ = t.beginRelease(7, 1, true);
    try testing.expectEqual(FrameStamp{ .appender_id = 0, .g = 4 }, seam.stampFor(seam.ctx, key.guestForestSlot(7)));
    // The screen: slot 7's lessee is 1 at g 4 (rule 4 armed); suspended
    // while its page is Recovering; the native leaf's recorded tail at g
    // 1 rides into rule 2; a leaf never recorded has none.
    var s = seam.screenFor(seam.ctx, key.guestForestSlot(7), 0x20000).?;
    try testing.expectEqual(@as(?u32, 1), s.appender_current);
    try testing.expectEqual(@as(u32, 4), s.g_current);
    try testing.expect(s.recorded_tail == null);
    try fp.screen.suspendLessee(1);
    s = seam.screenFor(seam.ctx, key.guestForestSlot(7), 0x20000).?;
    try testing.expectEqual(@as(?u32, null), s.appender_current);
    fp.screen.resumeLessee(1);
    s = seam.screenFor(seam.ctx, key.NATIVE_FOREST_SLOT, 0x30000).?;
    try testing.expectEqual(@as(u32, 2), s.g_current);
    try testing.expectEqual(@as(?u32, 0), s.appender_current);
    try testing.expectEqual(@as(u32, 1), s.recorded_tail.?.g);
    try testing.expectEqual(@as(u32, 12288), s.recorded_tail.?.tail);
    // A stale lessee's (x, 1) frame at/past the tail is rule 2; the
    // manager's (0, 2) past it is current.
    try testing.expectEqual(@as(?u8, 2), s.foreignRule(.{ .appender_id = 9, .g = 1 }, 12288, 1));
    try testing.expectEqual(@as(?u8, null), s.foreignRule(.{ .appender_id = 0, .g = 2 }, 12288, 1));
    try testing.expect(seam.screenFor(seam.ctx, key.NATIVE_FOREST_SLOT, 0x40000).?.recorded_tail == null);
    // A later release re-records the slot's set: the old leaf's entry goes.
    try t.recordTails(0, 2, &.{.{ .leaf_addr = 0x50000, .tail = 4096 }});
    try testing.expect(t.recordedTail(0, 0x30000) == null);
    try testing.expectEqual(@as(u32, 2), t.recordedTail(0, 0x50000).?.g);
    // The fence class rides through.
    fp.setFenced(true);
    try testing.expect(seam.screenFor(seam.ctx, key.guestForestSlot(7), 0x20000).?.pr_fenced);
}
