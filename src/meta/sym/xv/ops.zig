//! THE EXECUTOR of a cross-owner intent and the served step (charter §5.5
//! "Cross-owner namespace ops"; `docs/porting/sym_tokens_intents_
//! striping.md` §2.3, §2.6–§2.9): under the op's ALREADY-HELD travelling
//! set ("this machinery acquires nothing, which is exactly why it adds no
//! wait-for edge") — (1) step 0 + the intent: when step 0 is LOCAL the
//! `Put` rider rides its entry, else the intent is written ALONE into the
//! mount's own rotor slot first ("the initiator's ring is what the
//! roll-forward recovers, so nothing ships before the intent is durable
//! there"); registered IN FLIGHT before the record exists, `minted` once
//! durable; (2) the BARRIER of the initiator's ring (cross-DEVICE
//! ordering); (3) steps in plan order, each dispatched to the ONE applier
//! locally or shipped as ONE `XvStep` — the outcome classified by the arm
//! TAKEN, never by re-resolving the home after the fact; a `SlotMoved`
//! answer (the holder's door, or our own) re-resolves at the manager and
//! re-dispatches ≤ 2; a step whose slot moved TO the initiator takes its
//! own guards in the NON-PARKING form; (4) a LIVE witness refusal stops
//! the plan, COMPENSATES the applied steps in reverse under the ABORTING
//! flag (decision 0002 Q6) and answers the step's errno; a ship that
//! FAILS leaves the intent OPEN (`abandoned` on the register — the
//! cadence completes it), never fail-stops; (5) every local participant
//! barriered, then the RETIREMENT — synchronous, before the op releases
//! its guards. Phases `plan + intent_barrier + Σ ship_rtt + local_steps +
//! retire ≡ total` (stamped from the same instants).
//!
//! The roll-forward (§2.8): every intent HOME whose slot is local here,
//! ids ascending, the plan's guards acquired through the initiator's own
//! travelling acquisition, the record RE-READ under them (gone ⇒ retired
//! meanwhile), the FORWARD plan (or the INVERSE under ABORTING) through
//! the retrying dispatcher, barrier, retire; adoption by REGISTER STATE
//! never by elapsed time ("a live op's intent is its own however long it
//! runs"). The served side (§2.6): the lease check FIRST (`SlotMoved`,
//! never an errno the op surfaces), the wire `child` screen, the guard
//! verdict (`Covered` takes nothing; `Take` = `lockMany` of the step's
//! keys), the apply with `served = true`. PORTABLE, generic over `Store`.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const clock = @import("../../../core/clock.zig");
const log = @import("../../../core/log.zig");
const knobs = @import("../../../core/knobs.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const dlm = @import("../../dlm.zig");
const tree = @import("../../tree.zig");
const tx = @import("../../tx.zig");
const record = @import("../../record.zig");
const route_table = @import("../../routing/route_table.zig");
const economy = @import("../../economy.zig");
const token_cache = @import("../tokens/token_cache.zig");
const intent = @import("intent.zig");
const steps = @import("steps.zig");
const applier = @import("applier.zig");
const guards = @import("guards.zig");

const assert = std.debug.assert;
pub const Step = steps.Step;
pub const Plan = steps.Plan;
pub const Outcome = steps.Outcome;
pub const Link = token_cache.Link;
pub const AppenderId = u32;
pub const InodeValue = record.InodeValue;

pub const stats = struct {
    /// Steps shipped to a foreign holder / served at this holder (at
    /// rest fleet-wide `shipped ≡ served`).
    pub var xv_cross_owner_steps_shipped: counters.ShardedCounter = .{};
    pub var xv_cross_owner_steps_served: counters.ShardedCounter = .{};
    /// A served insert naming a child no holder has a record for or a
    /// slot nobody leases — "a dangling dentry a buggy peer would plant":
    /// refused EINVAL, must stay 0.
    pub var xv_cross_owner_steps_rejected: counters.Tripwire = .init("xv_cross_owner_steps_rejected");
    /// The phases (exact-sum: `plan + intent_barrier + Σship_rtt +
    /// local_steps + retire ≡ total`).
    pub var xv_cross_owner_phase_plan: counters.ShardedLatencyHistogram = .{};
    pub var xv_cross_owner_phase_intent_barrier: counters.ShardedLatencyHistogram = .{};
    pub var xv_cross_owner_phase_ship_rtt: counters.ShardedLatencyHistogram = .{};
    pub var xv_cross_owner_phase_local_steps: counters.ShardedLatencyHistogram = .{};
    pub var xv_cross_owner_phase_retire: counters.ShardedLatencyHistogram = .{};
    pub var xv_cross_owner_phase_total: counters.ShardedLatencyHistogram = .{};
    /// A step re-dispatched after a `SlotMoved` answer (the holder's door
    /// or our own); a stale writer era retried once.
    pub var xv_cross_owner_step_slot_moved_retries: counters.ShardedCounter = .{};
    pub var xv_cross_owner_step_stale_era_retries: counters.ShardedCounter = .{};
    /// A step whose slot moved TO the initiator mid-plan: its own guards
    /// taken in the non-parking form; a contended stripe is `SlotMoved`.
    pub var xv_cross_owner_step_late_guards: counters.ShardedCounter = .{};
    pub var xv_cross_owner_step_late_guard_refusals: counters.ShardedCounter = .{};
    /// A locally dispatched op whose door answered `SlotBusy` was
    /// re-dispatched ONCE through the cross-owner arm.
    pub var xv_cross_owner_op_slot_moved_redispatches: counters.ShardedCounter = .{};
    /// A local `None` on a slot a live foreign appender leases is NOT a
    /// witness: refused EAGAIN.
    pub var xv_cross_owner_witness_refusals: counters.ShardedCounter = .{};
    /// The dangling-name arm taken on an armed mount: must stay 0.
    pub var xv_cross_owner_dangling_names: counters.Tripwire = .init("xv_cross_owner_dangling_names");
    /// Plans started / completed / recovered by the roll-forward /
    /// compensated after a live refusal.
    pub var xv_started: counters.ShardedCounter = .{};
    pub var xv_completed: counters.ShardedCounter = .{};
    pub var xv_recovered: counters.ShardedCounter = .{};
    pub var xv_compensated: counters.ShardedCounter = .{};
    /// A ship that failed left the intent open for the cadence.
    pub var xv_abandoned: counters.ShardedCounter = .{};
    /// A LOCAL step's device error mid-plan (the fail-stop class): must
    /// stay 0.
    pub var xv_midplan_escalations: counters.Tripwire = .init("xv_midplan_escalations");
    /// Roll-forward passes the cadence ran.
    pub var xv_cadence_passes: counters.ShardedCounter = .{};
};

/// One legal move + one re-move; a third stale answer is the retryable
/// class the caller sees (the intent stays open).
pub const SLOT_MOVED_RETRIES: u32 = 2;

/// The cadence tick: the checkpoint LANDING ceiling (≈ 1.1 s) — an
/// abandoned intent completes within a few ceilings of the holder
/// returning.
pub const CADENCE_TICK_NS: u64 = 1_100 * std.time.ns_per_ms;

/// Where an object's slot is served from, as this mount sees it.
pub const Home = union(enum) {
    /// This mount leases the slot (or maintains it as the manager): the
    /// volume ordinal it lives on (a flat multi-volume set spans volumes
    /// with every slot local) and the slot's lease GENERATION (0 when no
    /// slot plane is armed) — the stamp a per-slot hint is valid under
    /// (the striping's negative cache: "a stale lease stamp reads as no
    /// hint").
    local: struct { volume: u16, g: u32 = 0 },
    foreign: struct { holder: AppenderId, link: Link },
    /// The lessee has no bound endpoint yet (or is dead): never dialled.
    unreachable_: struct { holder: AppenderId },
};

pub const Resolver = struct {
    ctx: *anyopaque,
    homeOf: *const fn (ctx: *anyopaque, slot: u16) Home,
    /// Re-resolve `slot` at the manager after a `SlotMoved` answer.
    refresh: *const fn (ctx: *anyopaque, slot: u16) Home,
    /// The slots this mount leases (the roll-forward's homes); the first
    /// is the mount's own rotor slot (the home of an intent whose step 0
    /// is foreign).
    localSlots: *const fn (ctx: *anyopaque, out: []u16) []u16,
    /// The dialled session to appender `holder` (the striping's supply
    /// asks name a CREATOR, not a slot); null = no endpoint bound.
    linkOf: *const fn (ctx: *anyopaque, holder: AppenderId) ?Link,
};

/// The directory striping's faces on the plane (charter §5.5 "Directory
/// striping"), bound by the filesystem's striping module BEFORE the wire
/// serves: a striping DOMAIN (a striped directory or a stripe) feeds no
/// census and no dominance window; the `SupplyStripeInos` served side
/// mints stripe records in THIS mount's slot on the directory's volume;
/// the explicit flip COMMAND shipped to the holder runs here.
pub const StripingFace = struct {
    ctx: *anyopaque,
    isDomain: *const fn (ctx: *anyopaque, ino: u64) bool,
    /// Mint one `S_IFDIR` stripe record per index into `out` (the
    /// records carry the directory's mode bits, uid, gid and stamp);
    /// returns how many were minted (a short count = the rest declined).
    supply: *const fn (ctx: *anyopaque, dir: u64, attrs: StripeAttrs, indices: []const u8, out: []u64) usize,
    /// `setfattr -n user.squeezefs.stripes -v K` served at the holder.
    flip: *const fn (ctx: *anyopaque, dir: u64, k: u8) errors.Error!void,
};

/// The attributes a supplied stripe record is minted with (its
/// directory's).
pub const StripeAttrs = struct { mode: u32, uid: u32, gid: u32, ts_ns: u64 };

/// The witness of a FOREIGN object read through the token divert (the
/// compensation's child type; null = unknown).
pub const Witness = struct {
    ctx: *anyopaque,
    readInode: *const fn (ctx: *anyopaque, global: u64) errors.Error!?InodeValue,
};

/// A served insert into a directory: the striping census' feed (the
/// CREATOR = the shipped child's slot holder).
pub const ServedInsertSink = struct {
    ctx: *anyopaque,
    note: *const fn (ctx: *anyopaque, parent: u64, creator: AppenderId) void,
};

/// A served ship by a wire peer on a slot this holder leases: the
/// handover judge's feed (charter §5.5 "the holder OFFERS slot s to
/// requester q"; `slots/plane.zig` `noteWireShip`). The peer travels as
/// its wire identity — the slot's volume resolves the appender id.
pub const ServedShipSink = struct {
    ctx: *anyopaque,
    note: *const fn (ctx: *anyopaque, volume: u16, slot: u16, peer: verbs.WireIdentity) void,
};

/// Test seams, read ONCE at the plane's init (never process statics).
pub const Seams = struct {
    /// The N-th ship (1-based) of this plane fails as a lost connection
    /// (0 = never): the intent stays open for the cadence.
    ship_fail_at: u32 = 0,
    /// The executor "dies" right after step N committed (N = 0: after
    /// step 0 + the intent; N ≥ len: before the retirement) — the kill -9
    /// window; `maxInt` = never.
    crash_after_step: u32 = std.math.maxInt(u32),
    /// The holder answers the next served step `SlotMoved` once.
    serve_slot_moved_once: bool = false,

    pub fn fromEnv(env: *const std.process.Environ.Map) Seams {
        return .{
            .ship_fail_at = knobs.intKnob("SQUEEZEFS_TEST_XV_SHIP_FAIL_AT", u32, env),
            .crash_after_step = knobs.optIntKnob("SQUEEZEFS_TEST_XV_CRASH_AFTER_STEP", u32, env) orelse std.math.maxInt(u32),
            .serve_slot_moved_once = knobs.boolKnob("SQUEEZEFS_TEST_XV_SERVE_SLOT_MOVED_ONCE", env),
        };
    }
};

pub const Error = error{
    OutOfMemory,
    Io,
    Corrupt,
    /// A slot moved past the re-dispatch bound: retryable, the intent open.
    SlotMoved,
    /// A ship failed: the intent is open, the cadence completes it.
    ShipFailed,
    Deferred,
    ValueTooLarge,
    NoSpace,
    /// A live witness refusal: the plan was compensated; the step's errno
    /// is in `Done.refused`.
    Refused,
    /// The crash seam.
    Crashed,
    PlanTooLarge,
    GuardRefused,
};

pub fn coreError(e: Error) errors.Error {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io, error.Corrupt => error.InputOutput,
        error.SlotMoved => error.SlotMoved,
        error.ShipFailed => error.HolderUnreachable,
        error.Deferred => error.Deferred,
        error.ValueTooLarge => error.ValueTooLarge,
        error.NoSpace => error.NoSpaceLeft,
        error.Refused, error.Crashed => error.InputOutput,
        error.PlanTooLarge => error.InvalidArgument,
        error.GuardRefused => error.StaleHolderView,
    };
}

/// The plan's result: every step's outcome (the reply is served from the
/// post-image of the step the op names).
pub const Done = struct {
    outcomes: [intent.XV_MAX_STEPS]Outcome = undefined,
    len: usize = 0,
    tx_id: u64,
    /// Set when the plan was compensated after a live refusal.
    refused: ?errors.Error = null,

    pub fn inode(self: *const Done, idx: usize) ?InodeValue {
        return self.outcomes[idx].inode;
    }
};

pub fn Plane(comptime Store: type) type {
    return struct {
        const Self = @This();
        pub const Apply = applier.Applier(Store);

        gpa: Allocator,
        io: Io,
        apply: Apply,
        locks: *dlm.Manager,
        parked: guards.ParkedScopes,
        own_id: AppenderId,
        volume: u16,
        resolver: Resolver,
        witness: ?Witness = null,
        served_insert: ?ServedInsertSink = null,
        striping: ?StripingFace = null,
        minter: intent.Minter,
        register: intent.Register,
        seams: Seams,
        /// The volume's record cap (`min(64 KiB, node_size/4)`).
        record_cap: usize,
        /// The directory striping width in force (`stripe.resolveK`; 1 =
        /// off) — the word `getfattr -n user.squeezefs.stripes` answers.
        stripe_k: u8 = 64,
        scope_seq: std.atomic.Value(u64) = .init(1),
        ships: std.atomic.Value(u32) = .init(0),
        serve_moved_armed: std.atomic.Value(bool),
        /// The cadence's last pass stamp (a pass runs only while the
        /// register holds an abandoned intent).
        last_pass_ns: u64 = 0,

        pub fn init(gpa: Allocator, io: Io, store: *Store, locks: *dlm.Manager, own_id: AppenderId, volume: u16, term: u64, resolver: Resolver, seams: Seams, record_cap: usize) Allocator.Error!Self {
            return .{
                .gpa = gpa,
                .io = io,
                .apply = Apply.init(gpa, io, store),
                .locks = locks,
                .parked = guards.ParkedScopes.init(gpa, io, locks),
                .own_id = own_id,
                .volume = volume,
                .resolver = resolver,
                .minter = intent.Minter.init(term),
                .register = try intent.Register.init(gpa, io, 1024),
                .seams = seams,
                .record_cap = record_cap,
                .serve_moved_armed = .init(seams.serve_slot_moved_once),
            };
        }

        pub fn deinit(self: *Self) void {
            self.parked.deinit();
            self.register.deinit(self.gpa);
        }

        fn homeOfIno(self: *Self, global: u64) Home {
            return self.resolver.homeOf(self.resolver.ctx, route_table.slotOf(global));
        }

        /// Do these objects' homes differ (two volumes, or a foreign
        /// holder among them)? The ONE question every namespace op asks
        /// before choosing the single-entry path or the intent.
        pub fn spans(self: *Self, inos_: []const u64) bool {
            if (inos_.len == 0) return false;
            const first = self.homeOfIno(inos_[0]);
            for (inos_[1..]) |g| if (!sameHome(first, self.homeOfIno(g))) return true;
            return false;
        }

        fn sameHome(a: Home, b: Home) bool {
            return switch (a) {
                .local => |x| b == .local and b.local.volume == x.volume,
                .foreign => |x| b == .foreign and b.foreign.holder == x.holder,
                .unreachable_ => false,
            };
        }

        /// Is `global`'s slot this mount's?
        pub fn isLocal(self: *Self, global: u64) bool {
            return self.homeOfIno(global) == .local;
        }

        /// The lease generation this mount holds `global`'s slot under
        /// (0 on an unarmed plane); null for a slot it does not hold.
        pub fn localGen(self: *Self, global: u64) ?u32 {
            return switch (self.homeOfIno(global)) {
                .local => |l| l.g,
                else => null,
            };
        }

        /// The holder of `global`'s slot as this mount sees it: null for a
        /// slot this mount holds.
        pub fn holderOf(self: *Self, global: u64) ?AppenderId {
            return switch (self.homeOfIno(global)) {
                .local => null,
                .foreign => |f| f.holder,
                .unreachable_ => |u| u.holder,
            };
        }

        /// Is `ino` a striped directory or a stripe (the striping face's
        /// word; a plane without the face knows none)?
        pub fn inStripingDomain(self: *Self, ino: u64) bool {
            const f = self.striping orelse return false;
            return f.isDomain(f.ctx, ino);
        }

        /// The travelling-guard tables for the op's keys.
        pub fn tables(self: *Self) guards.Tables {
            return .{ .ctx = self, .tableOf = &tableOf };
        }

        fn tableOf(ctx: *anyopaque, global: u64) guards.Table {
            const self: *Self = @ptrCast(@alignCast(ctx));
            return switch (self.homeOfIno(global)) {
                .local => .local,
                .foreign => |f| .{ .remote = .{ .holder = f.holder, .link = f.link } },
                .unreachable_ => |u| .{ .unreachable_ = u.holder },
            };
        }

        pub fn mintScope(self: *Self) guards.Scope {
            return guards.mintScope(self.scope_seq.fetchAdd(1, .monotonic));
        }

        /// The op's travelling set over the plan's keys (every parent
        /// exclusive + every name; every inode exclusive).
        pub fn acquirePlanGuards(self: *Self, plan: *const Plan, scope: guards.Scope, discovery: bool) Error!guards.Travelling {
            var inodes: [dlm.wide_requests]dlm.InodeReq = undefined;
            var dents: [dlm.wide_requests]dlm.DentryReq = undefined;
            var ni: usize = 0;
            var nd: usize = 0;
            for (plan.slice()) |s| {
                switch (s) {
                    .remove_dentry => |x| {
                        pushInode(&inodes, &ni, x.parent);
                        pushDentry(&dents, &nd, x.parent, x.name);
                    },
                    .insert_dentry => |x| {
                        pushInode(&inodes, &ni, x.parent);
                        pushDentry(&dents, &nd, x.parent, x.name);
                    },
                    else => pushInode(&inodes, &ni, s.homeIno()),
                }
            }
            return guards.acquire(self.gpa, self.io, self.locks, self.tables(), .{ .own_id = self.own_id, .volume = self.volume, .scope = scope, .inodes = inodes[0..ni], .dentries = dents[0..nd], .discovery = discovery }) catch |e| switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.PlanTooLarge => error.PlanTooLarge,
                error.GuardRefused, error.StaleHolderView => error.GuardRefused,
                error.HolderUnreachable => error.ShipFailed,
            };
        }

        fn pushInode(buf: []dlm.InodeReq, n: *usize, ino: u64) void {
            for (buf[0..n.*]) |r| if (r.ino == ino) return;
            if (n.* == buf.len) return;
            buf[n.*] = .{ .ino = ino, .mode = .exclusive };
            n.* += 1;
        }

        fn pushDentry(buf: []dlm.DentryReq, n: *usize, parent: u64, name: []const u8) void {
            for (buf[0..n.*]) |r| if (r.parent == parent and std.mem.eql(u8, r.name, name)) return;
            if (n.* == buf.len) return;
            buf[n.*] = .{ .parent = parent, .name = name, .mode = .exclusive };
            n.* += 1;
        }

        // ---- the witness at the holder ----

        /// The record of `global` AT ITS HOLDER: local off this store; a
        /// foreign object through the divert (`witness`). A local `null` on
        /// a slot a live foreign appender leases is NOT a witness.
        pub fn readWitness(self: *Self, global: u64) Error!?InodeValue {
            switch (self.homeOfIno(global)) {
                .local => return self.apply.readInode(global) catch |e| return mapApply(e),
                .foreign => {
                    const w = self.witness orelse {
                        stats.xv_cross_owner_witness_refusals.inc();
                        return error.ShipFailed;
                    };
                    return w.readInode(w.ctx, global) catch |e| return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        error.HolderUnreachable, error.StaleHolderView, error.SlotMoved, error.Deferred => error.ShipFailed,
                        else => error.Io,
                    };
                },
                .unreachable_ => {
                    stats.xv_cross_owner_witness_refusals.inc();
                    return error.ShipFailed;
                },
            }
        }

        fn mapApply(e: applier.Error) Error {
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                error.Corrupt => error.Corrupt,
                error.SlotBusy => error.SlotMoved,
                error.Deferred => error.Deferred,
                error.ValueTooLarge => error.ValueTooLarge,
                error.NoSpace, error.ChainFull => error.NoSpace,
            };
        }

        // ---- execute ----

        /// The intent's home for this plan: step 0's slot when local (the
        /// rider rides its entry), else the mount's own rotor slot.
        fn intentHomeFor(self: *Self, step0: Step) u64 {
            const slot0 = route_table.slotOf(step0.homeIno());
            if (self.homeOfIno(step0.homeIno()) == .local) return intent.homeIno(slot0);
            var buf: [1]u16 = undefined;
            const own = self.resolver.localSlots(self.resolver.ctx, &buf);
            return intent.homeIno(if (own.len == 0) 0 else own[0]);
        }

        /// Execute `plan` under the op's held guards (`scope` is the
        /// travelling scope; 0 = none travelled).
        pub fn execute(self: *Self, plan: *const Plan, scope: guards.Scope, now: u64) Error!Done {
            assert(plan.len > 0);
            const t0 = clock.monotonicNs();
            stats.xv_started.inc();
            const tx_id = self.minter.next();
            var done: Done = .{ .tx_id = tx_id };
            var recs: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
            var img_buf: [intent.IMAGE_MAX]u8 = undefined;
            const image = intent.encode(plan.op, 0, tx_id, plan.recs(&recs), self.record_cap, &img_buf) catch |e| return switch (e) {
                error.TooManySteps => error.PlanTooLarge,
                error.ValueTooLarge => error.ValueTooLarge,
            };
            const home = self.intentHomeFor(plan.steps[0]);
            const t1 = clock.monotonicNs();
            stats.xv_cross_owner_phase_plan.record(t1 - t0);
            // Step 0 + the intent, then the initiator's barrier.
            self.register.noteInFlight(tx_id, home, t0);
            errdefer self.register.noteAbandoned(tx_id);
            const rider: applier.Rider = .{ .put = .{ .home = home, .tx_id = tx_id, .image = image } };
            var first: usize = 0;
            if (self.homeOfIno(plan.steps[0].homeIno()) == .local and route_table.slotOf(plan.steps[0].homeIno()) == intent.slotOfHome(home)) {
                done.outcomes[0] = (try self.dispatch(tx_id, 0, plan.steps[0], rider, scope, now, .local_only)).out;
                done.len = 1;
                first = 1;
            } else {
                self.apply.commitRiderAlone(rider, .none) catch |e| return mapApply(e);
            }
            self.register.noteDurable(tx_id);
            self.apply.barrier() catch |e| return mapApply(e);
            const t2 = clock.monotonicNs();
            stats.xv_cross_owner_phase_intent_barrier.record(t2 - t1);
            if (first == 1) {
                try self.crashSeam(0);
                if (done.outcomes[0].status == .foreign_skipped) {
                    // Step 0's own witness refused (a racing mint): nothing
                    // applied before it; retire and answer its errno.
                    try self.retire(home, tx_id);
                    done.refused = plan.steps[0].foreignSkippedError();
                    stats.xv_compensated.inc();
                    return done;
                }
            }
            // The remaining steps in plan order; the instants CHAIN so the
            // phases sum to the total exactly.
            var i: usize = first;
            var prev = t2;
            while (i < plan.len) : (i += 1) {
                const d = try self.dispatch(tx_id, i, plan.steps[i], .none, scope, now, .any);
                const out = d.out;
                done.outcomes[i] = out;
                done.len = i + 1;
                const te = clock.monotonicNs();
                if (d.shipped) stats.xv_cross_owner_phase_ship_rtt.record(te - prev) else stats.xv_cross_owner_phase_local_steps.record(te - prev);
                prev = te;
                if (out.status == .foreign_skipped) {
                    // A live witness refusal: stop, compensate, answer the
                    // step's errno.
                    try self.compensate(tx_id, home, plan, i, &done, scope, now);
                    done.refused = plan.steps[i].foreignSkippedError();
                    stats.xv_compensated.inc();
                    self.register.forget(tx_id);
                    return done;
                }
                try self.crashSeam(@intCast(i));
            }
            try self.crashSeam(@intCast(plan.len));
            const t3 = prev;
            // Every local participant barriered, then the retirement.
            self.apply.barrier() catch |e| return mapApply(e);
            try self.retire(home, tx_id);
            const t4 = clock.monotonicNs();
            stats.xv_cross_owner_phase_retire.record(t4 - t3);
            stats.xv_cross_owner_phase_total.record(t4 - t0);
            stats.xv_completed.inc();
            return done;
        }

        fn crashSeam(self: *Self, after_step: u32) Error!void {
            if (self.seams.crash_after_step == after_step) return error.Crashed;
        }

        const Arm = enum { any, local_only };

        /// The outcome with the arm TAKEN (the classification law).
        const Dispatched = struct { out: Outcome, shipped: bool };

        /// Dispatch one step: own slot ⇒ the local applier; foreign ⇒ ONE
        /// `XvStep` to the holder; a `SlotMoved` answer from either arm
        /// re-resolves at the manager and dispatches again, ≤ 2.
        fn dispatch(self: *Self, tx_id: u64, idx: usize, step: Step, rider: applier.Rider, scope: guards.Scope, now: u64, arm: Arm) Error!Dispatched {
            var tries: u32 = 0;
            const slot = route_table.slotOf(step.homeIno());
            var home = self.resolver.homeOf(self.resolver.ctx, slot);
            while (true) {
                switch (home) {
                    .local => {
                        // A step whose slot moved TO us mid-plan (its guards
                        // travelled to the old holder) takes its own in the
                        // non-parking form; a fresh mint is exempt.
                        var late: tx.GuardSet = .none;
                        var contended = false;
                        if (tries > 0 and !step.isFreshMint()) {
                            stats.xv_cross_owner_step_late_guards.inc();
                            late = self.lateGuards(step) orelse blk: {
                                stats.xv_cross_owner_step_late_guard_refusals.inc();
                                contended = true;
                                break :blk .none;
                            };
                        }
                        if (!contended) {
                            if (self.apply.apply(step, rider, late, now)) |out| {
                                return .{ .out = out, .shipped = false };
                            } else |e| switch (e) {
                                error.SlotBusy => {},
                                error.Io, error.Corrupt => {
                                    stats.xv_midplan_escalations.fire("a local cross-owner step's device error");
                                    return mapApply(e);
                                },
                                else => return mapApply(e),
                            }
                        }
                    },
                    .foreign => |f| {
                        if (arm == .local_only) return error.SlotMoved;
                        if (try self.ship(f.link, tx_id, idx, step, scope)) |out| return .{ .out = out, .shipped = true };
                    },
                    .unreachable_ => return error.ShipFailed,
                }
                // The slot-moved class from either arm.
                if (tries == SLOT_MOVED_RETRIES) return error.SlotMoved;
                tries += 1;
                stats.xv_cross_owner_step_slot_moved_retries.inc();
                home = self.resolver.refresh(self.resolver.ctx, slot);
            }
        }

        /// A late step's own guards (try-only; the set travels into the
        /// step's entry).
        fn lateGuards(self: *Self, step: Step) ?tx.GuardSet {
            var inodes: [2]dlm.InodeReq = undefined;
            var dents: [1]dlm.DentryReq = undefined;
            var ni: usize = 0;
            var nd: usize = 0;
            switch (step) {
                .remove_dentry => |x| {
                    pushInode(&inodes, &ni, x.parent);
                    pushDentry(&dents, &nd, x.parent, x.name);
                },
                .insert_dentry => |x| {
                    pushInode(&inodes, &ni, x.parent);
                    pushDentry(&dents, &nd, x.parent, x.name);
                },
                else => pushInode(&inodes, &ni, step.homeIno()),
            }
            const set = (self.locks.tryLockMany(self.io, inodes[0..ni], dents[0..nd]) catch return null) orelse return null;
            const held = self.gpa.create(HeldSet) catch return null;
            held.* = .{ .io = self.io, .gpa = self.gpa, .set = set };
            return tx.GuardSet.init(held, &HeldSet.release);
        }

        const HeldSet = struct {
            io: Io,
            gpa: Allocator,
            set: dlm.GuardSet,
            fn release(ctx: *anyopaque) void {
                const self: *HeldSet = @ptrCast(@alignCast(ctx));
                self.set.release(self.io);
                self.gpa.destroy(self);
            }
        };

        /// ONE `XvStep` to the holder: the outcome, or null for the
        /// slot-moved class (re-resolve). A lost connection is a failed ship.
        fn ship(self: *Self, link: Link, tx_id: u64, idx: usize, step: Step, scope: guards.Scope) Error!?Outcome {
            const n = self.ships.fetchAdd(1, .monotonic) + 1;
            if (self.seams.ship_fail_at != 0 and n == self.seams.ship_fail_at) {
                stats.xv_abandoned.inc();
                return error.ShipFailed;
            }
            stats.xv_cross_owner_steps_shipped.inc();
            const rec = step.toRec();
            const rep = link.call(link.ctx, .{ .xv_step = .{ .head = .{ .volume = self.volume, .step_idx = @intCast(idx), .reserved = 0, .tx_id = tx_id, .scope = scope, .step = rec.w }, .name = rec.name } }) catch |e| switch (e) {
                error.SlotMoved, error.StaleHolderView => return null,
                error.StaleOwnerEra => {
                    // Retried ONCE with the same ids after the lane relearned the era.
                    stats.xv_cross_owner_step_stale_era_retries.inc();
                    const again = link.call(link.ctx, .{ .xv_step = .{ .head = .{ .volume = self.volume, .step_idx = @intCast(idx), .reserved = 0, .tx_id = tx_id, .scope = scope, .step = rec.w }, .name = rec.name } }) catch {
                        stats.xv_abandoned.inc();
                        return error.ShipFailed;
                    };
                    return outcomeOf(again.xv_step);
                },
                error.OutOfMemory => return error.OutOfMemory,
                error.Deferred => return error.Deferred,
                error.Rejected => return error.Corrupt,
                else => {
                    stats.xv_abandoned.inc();
                    return error.ShipFailed;
                },
            };
            return outcomeOf(rep.xv_step);
        }

        fn outcomeOf(w: verbs.StepOutcome) Outcome {
            return .{ .status = @fromBackingInt(w.status), .inode = if (w.has_inode != 0) (InodeValue.decode(&std.mem.toBytes(w.inode)) catch null) else null };
        }

        /// The live refusal's compensation: the ABORTING flip (one entry,
        /// barriered), every applied step before `refused_idx` inverted in
        /// REVERSE through the same applier, the retirement.
        fn compensate(self: *Self, tx_id: u64, home: u64, plan: *const Plan, refused_idx: usize, done: *Done, scope: guards.Scope, now: u64) Error!void {
            var recs: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
            var img_buf: [intent.IMAGE_MAX]u8 = undefined;
            const image = intent.encode(plan.op, intent.FLAG_ABORTING, tx_id, plan.recs(&recs), self.record_cap, &img_buf) catch return error.ValueTooLarge;
            self.apply.commitRiderAlone(.{ .put = .{ .home = home, .tx_id = tx_id, .image = image } }, .none) catch |e| return mapApply(e);
            self.apply.barrier() catch |e| return mapApply(e);
            var i = refused_idx;
            while (i > 0) {
                i -= 1;
                if (done.outcomes[i].status != .applied) continue;
                try self.invertOne(tx_id, i, plan.steps[i], scope, now);
            }
            self.apply.barrier() catch |e| return mapApply(e);
            self.apply.commitRiderAlone(.{ .delete = .{ .home = home, .tx_id = tx_id } }, .none) catch |e| return mapApply(e);
        }

        /// One step's inverse through the dispatcher; a minted child is
        /// destroyed (locally the record goes; a foreign mint is marked a
        /// corpse through its holder).
        fn invertOne(self: *Self, tx_id: u64, idx: usize, step: Step, scope: guards.Scope, now: u64) Error!void {
            switch (step) {
                .mint_inode, .create_inode => {
                    const ino = step.homeIno();
                    if (self.homeOfIno(ino) == .local) {
                        self.apply.destroyUnnamed(ino, .none) catch |e| return mapApply(e);
                    } else {
                        _ = try self.dispatch(tx_id, idx, .{ .set_nlink = .{ .ino = ino, .pre = 1, .post = 0, .ctime = now } }, .none, scope, now, .any);
                    }
                },
                .remove_dentry => |x| {
                    const ft: ?u32 = if (self.readWitness(x.expect_child) catch null) |v| v.mode & economy.S.IFMT else null;
                    _ = try self.dispatch(tx_id, idx, steps.inverse(step, ft).?, .none, scope, now, .any);
                },
                else => if (steps.inverse(step, null)) |inv| {
                    _ = try self.dispatch(tx_id, idx, inv, .none, scope, now, .any);
                },
            }
        }

        /// The retirement: the exact key's `Delete` (idempotent).
        fn retire(self: *Self, home: u64, tx_id: u64) Error!void {
            self.apply.commitRiderAlone(.{ .delete = .{ .home = home, .tx_id = tx_id } }, .none) catch |e| return mapApply(e);
            self.register.forget(tx_id);
        }

        // ---- the roll-forward ----

        /// Recovery at mount, before the mount serves: every home whose
        /// slot is local here, ids ascending; an undecodable record refuses
        /// the mount (`Corrupt`).
        pub fn recoverOpen(self: *Self, now: u64) Error!u32 {
            const slots_buf = self.gpa.alloc(u16, route_table.W) catch return error.OutOfMemory;
            defer self.gpa.free(slots_buf);
            const slots_ = self.resolver.localSlots(self.resolver.ctx, slots_buf);
            var recovered: u32 = 0;
            for (slots_) |s| recovered += try self.recoverHome(intent.homeIno(s), now, .mount);
            return recovered;
        }

        const Pass = enum { mount, cadence };

        fn recoverHome(self: *Self, home: u64, now: u64, pass: Pass) Error!u32 {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            self.apply.scanHome(home, &kvs) catch |e| return mapApply(e);
            var ids: [intent.XV_MAX_STEPS * 64]u64 = undefined;
            var n: usize = 0;
            for (kvs.items) |kv| {
                const id = intent.txIdOfKey(kv.key) orelse return error.Corrupt;
                var out: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
                _ = intent.decodeStored(kv.key, kv.value, &out) catch {
                    log.err("cross-owner intent 0x{x} under home {d} does not decode — refusing (an id-mismatched or torn record)", .{ id, home });
                    return error.Corrupt;
                };
                if (n == ids.len) break;
                ids[n] = id;
                n += 1;
            }
            std.mem.sort(u64, ids[0..n], {}, std.sort.asc(u64));
            if (pass == .cadence) self.register.forgetAbandonedAbsent(home, ids[0..n]);
            var recovered: u32 = 0;
            for (ids[0..n]) |id| {
                self.minter.recover(id);
                // Adoption by register STATE, never by elapsed time: a live
                // op's intent is its own however long it runs; one the
                // register never saw (another incarnation's) is adopted.
                switch (pass) {
                    .mount => self.register.adopt(id, home, now),
                    .cadence => if (self.register.stateOf(id)) |st| {
                        if (st == .in_flight) continue;
                    } else self.register.adopt(id, home, now),
                }
                self.register.noteTaken(id);
                self.recoverOne(home, id, now) catch |e| {
                    // Left open for the next pass (an unreachable holder, a
                    // moved slot past the bound); a device error is loud.
                    self.register.noteAbandoned(id);
                    switch (e) {
                        error.ShipFailed, error.SlotMoved, error.Deferred, error.GuardRefused => continue,
                        else => return e,
                    }
                };
                recovered += 1;
            }
            return recovered;
        }

        /// One intent: the plan's guards through the travelling
        /// acquisition, the record RE-READ under them, forward (or the
        /// inverse under ABORTING), barrier, retire.
        fn recoverOne(self: *Self, home: u64, tx_id: u64, now: u64) Error!void {
            const k = intent.recordKey(home, tx_id);
            const bytes0 = (self.apply.readIntentBytes(home, tx_id) catch |e| return mapApply(e)) orelse {
                self.register.forget(tx_id);
                return;
            };
            defer self.gpa.free(bytes0);
            var out: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
            var plan = Plan.fromIntent(intent.decodeStored(&k, bytes0, &out) catch return error.Corrupt);
            const scope = self.mintScope();
            var held = try self.acquirePlanGuards(&plan, scope, false);
            defer held.release(self.gpa);
            // Re-read under the guards: gone ⇒ retired meanwhile.
            const bytes = (self.apply.readIntentBytes(home, tx_id) catch |e| return mapApply(e)) orelse {
                self.register.forget(tx_id);
                return;
            };
            defer self.gpa.free(bytes);
            var out2: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
            const it = intent.decodeStored(&k, bytes, &out2) catch return error.Corrupt;
            if (it.steps.len != plan.len) return error.Corrupt;
            plan = Plan.fromIntent(it);
            if (it.aborting()) {
                var i = plan.len;
                while (i > 0) {
                    i -= 1;
                    try self.invertOne(tx_id, i, plan.steps[i], scope, now);
                }
            } else {
                for (plan.slice(), 0..) |s, i| {
                    _ = try self.dispatch(tx_id, i, s, .none, scope, now, .any);
                }
            }
            self.apply.barrier() catch |e| return mapApply(e);
            try self.retire(home, tx_id);
            stats.xv_recovered.inc();
        }

        /// A dead appender's recovery landed (charter §5.5 "Death and
        /// recovery … roll forward open intents"): the recovered slots are
        /// local homes now, so every home is scanned as a cadence pass —
        /// an intent the dead appender left open is adopted and completed.
        pub fn rollForwardRecovered(self: *Self, now: u64) Error!u32 {
            const slots_buf = self.gpa.alloc(u16, route_table.W) catch return error.OutOfMemory;
            defer self.gpa.free(slots_buf);
            const slots_ = self.resolver.localSlots(self.resolver.ctx, slots_buf);
            var recovered: u32 = 0;
            for (slots_) |s| recovered += try self.recoverHome(intent.homeIno(s), now, .cadence);
            self.register.publish(now);
            return recovered;
        }

        /// The cadence tick: the expiry sweep; a roll-forward pass only
        /// while the register holds an abandoned intent; the gauges.
        pub fn cadenceTick(self: *Self, now: u64) Error!u32 {
            _ = self.parked.sweepExpired(now);
            self.register.publish(now);
            if (!self.register.anyAbandoned()) return 0;
            stats.xv_cadence_passes.inc();
            self.last_pass_ns = now;
            const slots_buf = self.gpa.alloc(u16, route_table.W) catch return error.OutOfMemory;
            defer self.gpa.free(slots_buf);
            const slots_ = self.resolver.localSlots(self.resolver.ctx, slots_buf);
            var recovered: u32 = 0;
            for (slots_) |s| recovered += try self.recoverHome(intent.homeIno(s), now, .cadence);
            self.register.publish(now);
            return recovered;
        }

        // ---- the served side ----

        pub const ServeError = error{ SlotMoved, Rejected, Deferred, Io, OutOfMemory };

        /// `xv_serve_step` under the holder's door: the lease check FIRST,
        /// the insert's child screen, the guard verdict, the apply.
        pub fn serveStep(self: *Self, step: Step, scope: guards.Scope, requester: ?AppenderId, now: u64) ServeError!Outcome {
            const slot = route_table.slotOf(step.homeIno());
            if (self.resolver.homeOf(self.resolver.ctx, slot) != .local) return error.SlotMoved;
            if (self.serve_moved_armed.swap(false, .acq_rel)) return error.SlotMoved;
            if (step == .insert_dentry) {
                const child = step.insert_dentry.child;
                const admissible = switch (self.homeOfIno(child)) {
                    .local => (self.apply.readInode(child) catch return error.Io) != null,
                    .foreign => true,
                    .unreachable_ => true,
                };
                if (!admissible) {
                    stats.xv_cross_owner_steps_rejected.fire("a served insert names a child with no record");
                    return error.Rejected;
                }
            }
            var inodes: [2]dlm.InodeReq = undefined;
            var dents: [1]dlm.DentryReq = undefined;
            var ni: usize = 0;
            var nd: usize = 0;
            switch (step) {
                .remove_dentry => |x| {
                    pushInode(&inodes, &ni, x.parent);
                    pushDentry(&dents, &nd, x.parent, x.name);
                },
                .insert_dentry => |x| {
                    pushInode(&inodes, &ni, x.parent);
                    pushDentry(&dents, &nd, x.parent, x.name);
                },
                else => pushInode(&inodes, &ni, step.homeIno()),
            }
            var held: tx.GuardSet = .none;
            switch (self.parked.verdict(scope, inodes[0..ni], dents[0..nd])) {
                .covered => guards.stats.xv_cross_owner_steps_covered.inc(),
                .take => {
                    guards.stats.xv_cross_owner_steps_took_guards.inc();
                    const set = self.locks.lockManyUncancelable(self.io, inodes[0..ni], dents[0..nd]) catch return error.Rejected;
                    const h = self.gpa.create(HeldSet) catch return error.OutOfMemory;
                    h.* = .{ .io = self.io, .gpa = self.gpa, .set = set };
                    held = tx.GuardSet.init(h, &HeldSet.release);
                },
            }
            const out = self.apply.apply(step, .none, held, now) catch |e| return switch (e) {
                error.SlotBusy => error.SlotMoved,
                error.Deferred => error.Deferred,
                error.OutOfMemory => error.OutOfMemory,
                else => error.Io,
            };
            stats.xv_cross_owner_steps_served.inc();
            if (step == .insert_dentry and out.status == .applied) {
                // The census' CREATOR is the shipped child's slot holder
                // (a create in a foreign directory mints the child in the
                // creator's rotor); the requester's word stands in when the
                // child is ours. A striping domain feeds no census.
                const parent = step.insert_dentry.parent;
                const creator = self.holderOf(step.insert_dentry.child) orelse requester;
                if (self.served_insert) |s| if (creator) |r| if (!self.inStripingDomain(parent)) s.note(s.ctx, parent, r);
            }
            return out;
        }

        /// `LookupExact(parent, name)`: guard-free, off the local records.
        pub fn lookupExact(self: *Self, parent: u64, name: []const u8) ServeError!?applier.Found {
            if (self.homeOfIno(parent) != .local) return error.SlotMoved;
            return self.apply.findDentry(parent, name) catch error.Io;
        }
    };
}

/// The holder's wire face for `XvStep` and `LookupExact`.
pub fn StepService(comptime Store: type) type {
    return struct {
        const Self = @This();
        plane: *Plane(Store),
        /// The requester's appender id per peer identity (the census
        /// feed); null = unknown.
        requesterOf: ?*const fn (peer: verbs.WireIdentity) ?AppenderId = null,
        /// The handover judge's feed (every served step).
        served_ship: ?ServedShipSink = null,

        pub fn register(self: *Self, svc: *service.Service) void {
            svc.register(.xv_step, .{ .ctx = self, .serve = &serveStep });
            svc.register(.lookup_exact, .{ .ctx = self, .serve = &serveLookup });
        }

        fn serveStep(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
            const self: *Self = @ptrCast(@alignCast(ctx));
            const r = req.payload.xv_step;
            if (r.head.volume != self.plane.volume) return service.Answer.rejected("xv_step names a volume this node does not hold");
            if (!intent.stepWordsAdmissible(r.head.step)) return service.Answer.rejected("xv_step carries a word its kind never uses");
            const step = Step.fromRec(.{ .w = r.head.step, .name = r.name });
            const requester: ?AppenderId = if (self.requesterOf) |f| (if (req.peer) |p| f(p) else null) else null;
            const out = self.plane.serveStep(step, r.head.scope, requester, @bitCast(clock.realtimeCoarseNs())) catch |e| return switch (e) {
                // The slot-moved class is the block's "not holder" word
                // (`StaleHolderView` at the caller — a redirect, never a
                // loud refusal).
                error.SlotMoved => .{ .status = .{ .status = .not_holder } },
                error.Rejected => service.Answer.rejected("a served insert names a child no holder has a record for"),
                error.Deferred => service.Answer.deferred("the holder's door deferred the step"),
                error.OutOfMemory => service.Answer.refused(error.OutOfMemory, "out of memory serving the step"),
                error.Io => service.Answer.refused(error.InputOutput, "the holder's records could not be read or written"),
            };
            // "A ship into a striping domain feeds no window" (charter
            // §5.5): a striped directory's or a stripe's ships never earn
            // a handover offer.
            if (self.served_ship) |ss| if (req.peer) |p| if (!self.plane.inStripingDomain(step.homeIno())) ss.note(ss.ctx, r.head.volume, route_table.slotOf(step.homeIno()), p);
            var w: verbs.StepOutcome = .{ .status = @backingInt(out.status), .has_inode = 0, .reserved = @splat(0), .inode = std.mem.zeroes(verbs.InodeValueWire), .reserved2 = @splat(0) };
            if (out.inode) |v| {
                w.has_inode = 1;
                w.inode = std.mem.bytesToValue(verbs.InodeValueWire, &v.encode());
            }
            return .{ .ok = .{ .xv_step = w } };
        }

        fn serveLookup(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
            const self: *Self = @ptrCast(@alignCast(ctx));
            const r = req.payload.lookup_exact;
            if (r.head.volume != self.plane.volume) return service.Answer.rejected("lookup_exact names a volume this node does not hold");
            const found = self.plane.lookupExact(r.head.parent, r.bytes) catch |e| return switch (e) {
                error.SlotMoved => .{ .status = .{ .status = .not_holder } },
                else => service.Answer.refused(error.InputOutput, "the holder's records could not be read"),
            };
            if (found) |f| return .{ .ok = .{ .lookup_exact = .{ .present = 1, .file_type = f.file_type, .reserved = @splat(0), .ino = f.child } } };
            return .{ .ok = .{ .lookup_exact = .{ .present = 0, .file_type = 0, .reserved = @splat(0), .ino = 0 } } };
        }
    };
}

/// The caller side of `LookupExact` over a link: the child ino, or null.
pub fn lookupExactRemote(link: Link, volume: u16, parent: u64, name: []const u8) errors.Error!?u64 {
    const rep = try link.call(link.ctx, .{ .lookup_exact = .{ .head = .{ .volume = volume, .name_len = @intCast(name.len), .reserved = 0, .reserved2 = 0, .parent = parent }, .bytes = name } });
    const d = rep.lookup_exact;
    return if (d.present != 0) d.ino else null;
}
