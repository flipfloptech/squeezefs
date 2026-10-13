//! Slot ACQUISITION and MINTING (charter §5.5 "Slot acquisition and
//! minting (first-writer-takes-it, load-bounded parent affinity)";
//! `docs/porting/sym_slot_leases.md` §2, §3.4, §4.8): the derivations with
//! their ties (`M`, `A_max`, the rotor cap), the rotor's `prefer:
//! unleased-then-idle`, the mint decision (`affinity → strictly-over →
//! overflow/smallest → signed-headroom rotor`), `gather`, cursors as
//! FLOORS, the cadence's three decisions (forced shrink, LRU release,
//! region release) — every one a pure function — and the manager's five
//! slot VERBS (`AcquireSlots/AcquireSlot/OfferSlot/ReleaseSlot/
//! ResolveSlot`) over the manager's volume seam, the lease table and the
//! door, with their durable step order (spec §1.3 T1/T2/T5), the
//! idempotency witnesses and the wire screens, bound into the manager's
//! `HandlerTable`. PORTABLE: the device-bound effects of a grant on this
//! process's regions (the tree adopted, the cursor cell floored, the
//! frontier reset) and the in-process accept's transfer ride `Seams`.
//!
//! T1's durable order: RAM `g += 1, leased{q}` → ONE tree-0 control entry
//! `Leased{q, g, grant-time words}` per slot of the call, BARRIERED → the
//! install → (a failed write rolls the RAM entry back keeping the minted
//! `g`). T5's: the verdict against tree 0's record → ONE entry `Unleased
//! {words, g, last_written, seq_floor}` + `slot_tails` → ring 0's seq
//! offset raised above the departing floor → the RAM drop. Both admit in
//! the USER class; a full window answers `Deferred` and kicks the
//! checkpoint (the manager's law).

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const derive = @import("../../../core/derive.zig");
const log = @import("../../../core/log.zig");
const verbs = @import("../../../wire/verbs.zig");
const screen = @import("../../../wire/screen.zig");
const codec = @import("../../../wire/codec.zig");
const key = @import("../../key.zig");
const inos = @import("../../inos.zig");
const journal = @import("../../journal.zig");
const geometry = @import("../../geometry.zig");
const cr = @import("../control_records.zig");
const manager = @import("../manager.zig");
const tails_spill = @import("tails_spill.zig");
const page = @import("../appender_page.zig");
const lt = @import("lease_table.zig");
const door_mod = @import("door.zig");

const assert = std.debug.assert;
pub const LeaseTable = lt.LeaseTable;
pub const Door = door_mod.Door;
pub const AppenderId = lt.AppenderId;
pub const SlotWords = lt.SlotWords;

pub const stats = struct {
    /// Grants written (every slot of every `AcquireSlot(s)`).
    pub var slot_grants: counters.ShardedCounter = .{};
    /// An explicit ask for a slot another appender holds (legal).
    pub var slot_acquire_refusals: counters.ShardedCounter = .{};
    /// Of those: a first touch in a handover's release → grant gap, the
    /// slot reserved for its accepter (the requester's own served ships
    /// at the releasing holder used to take the slot back).
    pub var slot_acquire_reserved_refusals: counters.ShardedCounter = .{};
    /// Of those: a first touch of a recovered slot inside its coherence
    /// hold (decision 0003 §U.2: a live member may still serve the dead
    /// holder's cached token until its census pull retires the holder).
    pub var slot_acquire_coherence_refusals: counters.ShardedCounter = .{};
    /// A rotor ask past `2 × M` (legal).
    pub var slot_rotor_cap_refusals: counters.ShardedCounter = .{};
    /// Releases that landed (`ReleaseSlot` and the in-process accept's).
    pub var slot_releases: counters.ShardedCounter = .{};
    /// `ResolveSlot` served (0 on a solo mount).
    pub var slot_resolve_rpcs: counters.ShardedCounter = .{};
    /// Mints by the mint decision's arms.
    pub var affinity_mints: counters.ShardedCounter = .{};
    pub var rotor_mints: counters.ShardedCounter = .{};
    pub var dir_gather_mints: counters.ShardedCounter = .{};
    /// The parent WAS leased and non-native but at/over the ceiling.
    pub var affinity_ceiling_spills: counters.ShardedCounter = .{};
    /// Every rotor tree strictly over the ceiling: one more rotor slot.
    pub var affinity_ceiling_overflows: counters.ShardedCounter = .{};
    /// The ceiling in force, bytes.
    pub var affinity_a_max_bytes: counters.Gauge = .{};
    /// `M` in force.
    pub var slot_rotor: counters.Gauge = .{};
    /// The cadence's three arms.
    pub var slot_lru_releases: counters.ShardedCounter = .{};
    pub var slot_forced_shrinks: counters.ShardedCounter = .{};
    pub var slot_region_releases: counters.ShardedCounter = .{};
    /// `Live` page entries below tree 0's `g` dropped at the arm.
    pub var slot_lease_stale_entries: counters.ShardedCounter = .{};
    /// A joiner's tree-0 projection rows naming its OWN appender id at the
    /// joined arm — a previous incarnation's leases (the hint hands a
    /// rejoin the same id), loaded unleased (0003 §T.2).
    pub var slot_projection_stale_self_rows: counters.ShardedCounter = .{};
    /// Two live attestations of one slot (C14's live face): must stay 0.
    pub var slot_lease_conflicts: counters.Tripwire = .init("slot_lease_conflicts");
};

// ---------------------------------------------------------------------------
// Derivations, with their ties.
// ---------------------------------------------------------------------------

/// The shipped granularity: a volume's load divisible into ≥ 64 movable
/// slices from birth (charter §5.4 "Mint spread").
pub const MINT_SPREAD: u64 = 64;
/// The routing width (the u16 slot id).
pub const W: u64 = lt.W;

/// `M = clamp(W / (2 × writers_known), 1, MINT_SPREAD)`: `2 ×` keeps half
/// the width unleased for inheritance and churn.
pub fn mintSlotsDerived(writers_known: u64) u64 {
    return derive.clamp(W / (2 * @max(writers_known, 1)), 1, MINT_SPREAD);
}

/// `SQUEEZEFS_SYM_MINT_SLOTS` wins verbatim (the registry ranged it).
pub fn resolveMintSlots(explicit: ?u64, writers_known: u64) u64 {
    return explicit orelse mintSlotsDerived(writers_known);
}

/// The rotor cap: an over-cap rotor may ask up to `2 × M` — inside the
/// width headroom the `2 ×` in `M` reserved.
pub fn rotorCap(m: u64) u64 {
    return 2 * m;
}

/// `A_max(t) = max(used_leaf_bytes / MINT_SPREAD, node_size)`: no tree
/// holds more than 1/64 of the volume's current leaf bytes, never less
/// than one extent.
pub fn affinityCeiling(used_leaf_bytes: u64, node_size: u64) u64 {
    return @max(used_leaf_bytes / MINT_SPREAD, node_size);
}

pub const CeilingRefusal = error{ BelowNodeSize, AboveHeap };

/// `SQUEEZEFS_SYM_AFFINITY_MAX_MB` is the whole law when present: a static
/// ceiling below `node_size` or above THIS volume's heap REFUSES the mount
/// naming the bound (spec §9 Q13 — never an in-process clamp).
pub fn resolveAffinityCeiling(explicit_mib: ?u64, used_leaf_bytes: u64, node_size: u64, heap_len: u64) CeilingRefusal!u64 {
    const mib = explicit_mib orelse return affinityCeiling(used_leaf_bytes, node_size);
    const bytes = mib * derive.MiB;
    if (bytes < node_size) return error.BelowNodeSize;
    if (bytes > heap_len) return error.AboveHeap;
    return bytes;
}

/// Cursors travel with the lease as FLOORS (charter §5.5 "never re-mint"):
/// `max(tree 0, page, the window's highest minted ino + 1, the high live
/// key + 1)`. The recorded words are NEXT values already; the window and
/// the live key are inos, so they step by one.
pub fn cursorFloor(tree0_cursor: u64, page_cursor: ?u64, window_max_ino: ?u64, high_live_ino: ?u64) u64 {
    var c = @max(tree0_cursor, inos.LOCAL_INO_BASE);
    if (page_cursor) |p| c = @max(c, p);
    if (window_max_ino) |m| c = @max(c, m +| 1);
    if (high_live_ino) |h| c = @max(c, h +| 1);
    return c;
}

// ---------------------------------------------------------------------------
// The mint decision (spec §2.4 `mint_choice`).
// ---------------------------------------------------------------------------

pub const RotorTree = struct { slot: u16, bytes: u64 };

pub const MintInputs = struct {
    /// The parent's slot and whether THIS mount leases it as a non-native slot.
    parent_slot: u16,
    parent_leased_non_native: bool,
    parent_tree_bytes: u64,
    a_max: u64,
    node_size: u64,
    /// The rotor trees this mount leases (slot, live bytes).
    rotor: []const RotorTree,
    /// `2 × M`.
    rotor_cap: u64,
    /// The round-robin cursor (ties).
    rr: u64,
};

pub const MintChoice = union(enum) {
    /// Into the parent's slot.
    affinity: u16,
    /// Every rotor tree strictly over the cap and the rotor under `2M`:
    /// ask the manager for one more rotor slot.
    overflow,
    /// Every rotor tree over the cap at `2M`: the smallest rotor tree.
    smallest: u16,
    /// The rotor tree with the most SIGNED headroom (the least-over tree
    /// takes the mint once every tree is at or over the cap).
    rotor: u16,
    /// No rotor at all (the arm's window): the parent's slot regardless.
    none,

    pub fn slot(self: MintChoice) ?u16 {
        return switch (self) {
            .affinity, .smallest, .rotor => |s| s,
            .overflow, .none => null,
        };
    }
};

pub fn mintChoice(in: MintInputs) MintChoice {
    // 1. Affinity: the parent's own tree below the soft ceiling.
    if (in.parent_leased_non_native and in.parent_tree_bytes < in.a_max) return .{ .affinity = in.parent_slot };
    if (in.rotor.len == 0) return .none;
    // 2. Every rotor tree STRICTLY over the cap by ≥ 1 extent (exact
    // equality is NOT over).
    var all_over = true;
    for (in.rotor) |r| {
        if (r.bytes < in.a_max + in.node_size) {
            all_over = false;
            break;
        }
    }
    if (all_over) {
        if (in.rotor.len < in.rotor_cap) return .overflow;
        var best = in.rotor[in.rr % in.rotor.len];
        for (in.rotor) |r| if (r.bytes < best.bytes) {
            best = r;
        };
        return .{ .smallest = best.slot };
    }
    // 3. The most signed headroom `a_max − bytes`, ties by the rr cursor.
    var best_i = in.rr % in.rotor.len;
    var best_h: i128 = @as(i128, in.a_max) - in.rotor[best_i].bytes;
    for (in.rotor, 0..) |r, i| {
        const h: i128 = @as(i128, in.a_max) - r.bytes;
        if (h > best_h) {
            best_h = h;
            best_i = i;
        }
    }
    return .{ .rotor = in.rotor[best_i].slot };
}

/// The counters a choice feeds (the overflow arm counts its own grant).
/// Closure: `affinity_mints + rotor_mints + dir_gather_mints ≡ mints`.
pub fn countChoice(c: MintChoice, parent_leased_non_native: bool) void {
    switch (c) {
        .affinity => stats.affinity_mints.inc(),
        .rotor, .smallest => {
            stats.rotor_mints.inc();
            if (parent_leased_non_native) stats.affinity_ceiling_spills.inc();
        },
        .overflow, .none => {},
    }
}

// ---------------------------------------------------------------------------
// `gather` (spec §2.4): the ONE user-settable name of the reserved family.
// ---------------------------------------------------------------------------

pub const GATHER_XATTR = "user.squeezefs.gather";

/// The bool-knob vocabulary: `1/true/yes/on`, trimmed, case-insensitive.
pub fn gatherOptIn(value: []const u8) bool {
    const v = std.mem.trim(u8, value, " \t\r\n");
    inline for (.{ "1", "true", "yes", "on" }) |word| if (std.ascii.eqlIgnoreCase(v, word)) return true;
    return false;
}

/// The gather arm runs FIRST: null (the ordinary policy) when the parent
/// is on another volume, in the native slot, or carries no opt-in; else
/// the parent's slot whatever the ceiling says. Never automatic.
pub fn gatherMintSlot(parent_on_this_volume: bool, parent_native: bool, opt_in: bool, parent_slot: u16) ?u16 {
    if (!parent_on_this_volume or parent_native or !opt_in) return null;
    stats.dir_gather_mints.inc();
    return parent_slot;
}

// ---------------------------------------------------------------------------
// The cadence's decisions (spec §4.8), pure.
// ---------------------------------------------------------------------------

pub const HeldSlot = struct { slot: u16, idle: bool, rotor: bool, native: bool, extents: u32 };

/// Forced shrink: the rotor past the re-derived `M` releases its IDLE
/// rotor slots, smallest trees first, until the rotor is at `M`.
pub fn forcedShrinkVictims(held: []const HeldSlot, m: u64, out: []u16) []u16 {
    var rotor_n: u64 = 0;
    for (held) |h| rotor_n += @intFromBool(h.rotor);
    if (rotor_n <= m) return out[0..0];
    var excess = rotor_n - m;
    var n: usize = 0;
    // Insertion-sorted by extents ascending among the idle rotor slots.
    var cands: [lt.LeaseTable.PICK_MAX]HeldSlot = undefined;
    var cn: usize = 0;
    for (held) |h| {
        if (!h.rotor or !h.idle or h.native) continue;
        var i = cn;
        while (i > 0 and (h.extents < cands[i - 1].extents or (h.extents == cands[i - 1].extents and h.slot < cands[i - 1].slot))) : (i -= 1) {
            if (i < cands.len) cands[i] = cands[i - 1];
        }
        if (i < cands.len) {
            cands[i] = h;
            if (cn < cands.len) cn += 1;
        }
    }
    for (cands[0..cn]) |c| {
        if (excess == 0 or n == out.len) break;
        out[n] = c.slot;
        n += 1;
        excess -= 1;
    }
    return out[0..n];
}

/// LRU release: a region holding more than the page budget releases idle
/// NON-rotor, non-native slots (ascending by slot) until at the budget.
pub fn lruReleaseVictims(held: []const HeldSlot, budget: usize, out: []u16) []u16 {
    if (held.len <= budget) return out[0..0];
    var excess = held.len - budget;
    // Insertion-sorted ascending among the eligible (bounded by `out`).
    var n: usize = 0;
    for (held) |h| {
        if (!h.idle or h.rotor or h.native) continue;
        var i = n;
        while (i > 0 and h.slot < out[i - 1]) : (i -= 1) {
            if (i < out.len) out[i] = out[i - 1];
        }
        if (i < out.len) {
            out[i] = h.slot;
            if (n < out.len) n += 1;
        }
    }
    const take = @min(n, excess);
    excess -= take;
    return out[0..take];
}

/// Region release: a declared region (never region 0) with no lease left,
/// its ring drained (`head == reusable_upto`), no pass inside and no window
/// in flight goes Free.
pub fn regionReleasable(appender_id: AppenderId, leases_left: usize, head: u64, reusable_upto: u64, pass_inside: bool, windows_inflight: u64) bool {
    return appender_id != journal.FIXED_RING_APPENDER and leases_left == 0 and head == reusable_upto and !pass_inside and windows_inflight == 0;
}

// ---------------------------------------------------------------------------
// The verbs.
// ---------------------------------------------------------------------------

pub const Error = error{
    /// The screen refused a wire word.
    Rejected,
    /// The durable witness contradicts the caller.
    Refused,
    /// Nothing written; retry (the structural door's moving tail, a full
    /// USER window).
    Deferred,
    /// A concurrent release or transfer holds the slot.
    Busy,
    /// A rotor ask past `2 × M`.
    RotorAtCap,
    Io,
    Corrupt,
    OutOfMemory,
};

pub const Admit = enum { try_, park };

/// The device-bound effects of the verbs on THIS process (bind.zig).
pub const Seams = struct {
    ctx: *anyopaque,
    /// The hosted set minus the native slot, ascending (owned by `gpa`).
    rotorCandidates: *const fn (ctx: *anyopaque, gpa: Allocator) Allocator.Error![]u16,
    nativeSlot: *const fn (ctx: *anyopaque) ?u16,
    /// This process's regions over the volume.
    ownsRegion: *const fn (ctx: *anyopaque, id: AppenderId) bool,
    /// Ring 0's `reusable_upto` (the structural door's tail at entry).
    ring0Tail: *const fn (ctx: *anyopaque) u64,
    /// A release to the manager raises ring 0's seq offset above the
    /// departing ring's floor (the manager's structural custody stamps
    /// above it).
    raiseRing0SeqFloor: *const fn (ctx: *anyopaque, floor: u64) void,
    /// An own region's install (`fresh` = a grant, not a re-adoption):
    /// the tree at the words' root, the cursor cell floored, the frontier
    /// reset to the receiving ring's head, the holder cache taught.
    install: *const fn (ctx: *anyopaque, slot: u16, holder: AppenderId, g: u32, words: SlotWords, fresh: bool) Error!void,
    /// After a release landed: the slot's tree is the manager's.
    released: *const fn (ctx: *anyopaque, slot: u16) void,
    /// A grant to a WIRE appender landed: the slot's tree (an unleased
    /// tree this process maintained) is the grantee's — out of this
    /// process's cache, read under its tokens from here on. Null = the
    /// RAM harnesses.
    foreignGranted: ?*const fn (ctx: *anyopaque, slot: u16) void = null,
    /// The in-process accept: the holder's flush-then-transfer to `to`
    /// (handover.zig's `Transfer` run by the plane); answers once tree 0
    /// names the slot `Unleased`.
    transfer: *const fn (ctx: *anyopaque, slot: u16, holder: AppenderId, to: AppenderId) Error!void,
    /// The PARKING admission a first touch takes HOLDING NOTHING (spec
    /// §2.3): park for `bytes` of ring-0 USER room, then release it (the
    /// control entry admits again under the mutex; the reservation made
    /// the room). `null` = the volume has no park seam (tests).
    parkForRoom: ?*const fn (ctx: *anyopaque, bytes: u64) Error!void = null,
};

pub const Grant = struct { slot: u16, g: u32, words: SlotWords };

pub const AcquireReq = struct {
    appender_id: AppenderId,
    /// Rotor slots wanted (0 = none; `explicit` alone).
    want: u16 = 0,
    explicit: []const u16 = &.{},
    admit: Admit = .try_,
    /// The explicit slots are rotor-marked (the arm's top-up).
    rotor: bool = false,
};

pub const AcquireOutcome = union(enum) {
    granted: struct { grants: []Grant, already: u16 },
    /// An explicit slot another appender holds (naming it) — the typed
    /// `SlotBusy` the door answers.
    refused: struct { slot: u16, holder: AppenderId, g: u32 },
};

pub const Resolved = union(enum) {
    unleased: struct { g: u32 },
    holder: struct { appender_id: AppenderId, g: u32, dead: bool },
};

pub const Verbs = struct {
    gpa: Allocator,
    io: Io,
    mgr: *manager.Manager,
    table: *LeaseTable,
    door: *Door,
    seams: Seams,
    /// One handover at a time per volume (the accept, the cadence's
    /// release); taken with NO 4a guard and never under the SMO mutex.
    handover_lock: Io.Mutex = .init,
    /// `M` in force (re-derived at every cadence tick).
    m: u64,
    /// `A_max` in force, bytes.
    a_max: u64,
    /// An offer to a WIRE requester rides its next renewal grant (the
    /// shard owner's carriage, charter §5.5 "the holder OFFERS slot s to
    /// requester q"); bound by the wire plane on the manager, null on a
    /// RAM harness. An in-process requester reads the table directly.
    offer_push: ?OfferPush = null,

    pub const OfferPush = struct {
        ctx: *anyopaque,
        push: *const fn (ctx: *anyopaque, to: AppenderId, slot: u16, holder: AppenderId) void,
    };

    pub fn init(gpa: Allocator, io: Io, mgr: *manager.Manager, table: *LeaseTable, door: *Door, seams: Seams, m: u64, a_max: u64) Verbs {
        stats.slot_rotor.set(m);
        stats.affinity_a_max_bytes.set(a_max);
        return .{ .gpa = gpa, .io = io, .mgr = mgr, .table = table, .door = door, .seams = seams, .m = m, .a_max = a_max };
    }

    pub fn setM(self: *Verbs, m: u64) void {
        self.m = m;
        stats.slot_rotor.set(m);
    }

    pub fn setCeiling(self: *Verbs, a_max: u64) void {
        self.a_max = a_max;
        stats.affinity_a_max_bytes.set(a_max);
    }

    // ---- tree 0 ----

    fn readState(self: *Verbs, slot: u16) Error!?cr.SlotState {
        const raw = (self.mgr.vol.vt.lookup(self.mgr.vol.ctx, self.gpa, cr.slotStateKey(slot).slice()) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        }) orelse return null;
        defer self.gpa.free(raw);
        return cr.SlotStateWire.decode(raw) catch return error.Corrupt;
    }

    fn control(self: *Verbs, recs: []const manager.ControlRecord) Error!void {
        self.mgr.vol.vt.controlEntry(self.mgr.vol.ctx, recs) catch |e| return switch (e) {
            error.JournalReserveExhausted => blk: {
                self.mgr.vol.vt.kickCheckpoint(self.mgr.vol.ctx);
                break :blk error.Deferred;
            },
            error.ValueTooLarge, error.Corrupt => error.Corrupt,
            error.Io => error.Io,
            error.OutOfMemory => error.OutOfMemory,
        };
    }

    fn requireLease(self: *Verbs) Error!void {
        if (self.mgr.lease != .held) return error.Refused;
    }

    /// The grant-time words of an unleased slot: tree 0's record, else the
    /// virgin words (an empty slot owns no extent).
    fn grantWords(self: *Verbs, slot: u16) Error!SlotWords {
        const s = (try self.readState(slot)) orelse return .{};
        return .{ .root = s.root, .cursor = @max(s.cursor, inos.LOCAL_INO_BASE), .extents = s.slot_tree_extents, .seq_floor = s.seq_floor };
    }

    // ---- AcquireSlots / AcquireSlot (T1) ----

    /// Screen → the PARKING admission (holding nothing) → the verb mutex
    /// → the candidates (explicit ∪ the rotor pick) → the rotor cap → the
    /// structural door for a grantee that is not the manager → RAM `g +=
    /// 1` → ONE control entry → the install. `grants` is owned by `gpa`.
    pub fn acquireSlots(self: *Verbs, gpa: Allocator, req: AcquireReq) Error!AcquireOutcome {
        try self.requireLease();
        if (req.want > screen.SLOTS_WANT_MAX) return error.Rejected;
        for (req.explicit) |s| if (!screen.slot(s).isOk()) return error.Rejected;
        // The parking admission BEFORE the mutex (spec §2.3, Issue 24): the
        // worst case is one `Leased` put per slot the call can grant.
        if (req.admit == .park) if (self.seams.parkForRoom) |park| {
            const puts: u64 = req.explicit.len + req.want;
            try park(self.seams.ctx, journal.ENTRY_HDR_LEN + puts * journal.recordFrameLen(cr.Tag.slot_state.keyLen(), cr.SlotStateWire.LEN));
        };
        self.mgr.verbs_lock.lockUncancelable(self.io);
        defer self.mgr.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        var picked: std.ArrayList(u16) = .empty;
        defer picked.deinit(gpa);
        var already: u16 = 0;
        // Explicit slots first: a held one by the requester is a replay; by
        // another appender a refusal naming it.
        for (req.explicit) |s| {
            const v = self.table.view(s);
            if (v.state == .unleased) {
                // A slot released by a handover stays RESERVED for its
                // accepter while the offer's stand holds: another first
                // touch in the release → grant gap is refused naming it.
                if (self.table.reservedFor(s, clock.monotonicNs())) |to| if (to != req.appender_id) {
                    stats.slot_acquire_refusals.inc();
                    stats.slot_acquire_reserved_refusals.inc();
                    return .{ .refused = .{ .slot = s, .holder = to, .g = v.g } };
                };
                // A recovered slot inside its coherence hold: no write
                // until every member's census retired the dead holder;
                // the refusal names the manager (its maintainer).
                if (self.table.coherenceHeldUntil(s, clock.monotonicNs()) != null) {
                    stats.slot_acquire_refusals.inc();
                    stats.slot_acquire_coherence_refusals.inc();
                    return .{ .refused = .{ .slot = s, .holder = lt.MANAGER, .g = v.g } };
                }
                try picked.append(gpa, s);
            } else if (v.holder == req.appender_id and v.state != .migrating) {
                already += 1;
            } else {
                stats.slot_acquire_refusals.inc();
                return .{ .refused = .{ .slot = s, .holder = v.holder, .g = v.g } };
            }
        }
        // The rotor ask: unleased-then-idle from the hosted candidates.
        if (req.want > 0) {
            const held_rotor = self.table.rotorHeldBy(req.appender_id);
            if (held_rotor + req.want > rotorCap(self.m)) {
                stats.slot_rotor_cap_refusals.inc();
                return error.RotorAtCap;
            }
            const cands = try self.seams.rotorCandidates(self.seams.ctx, gpa);
            defer gpa.free(cands);
            var buf: [lt.LeaseTable.PICK_MAX]u16 = undefined;
            const n: usize = @min(req.want, buf.len);
            for (self.table.pickUnleased(cands, buf[0..n])) |s| {
                var dup = false;
                for (picked.items) |p| dup = dup or p == s;
                if (!dup) try picked.append(gpa, s);
            }
        }
        const grants = try gpa.alloc(Grant, picked.items.len);
        errdefer gpa.free(grants);
        if (picked.items.len == 0) {
            if (already != 0) manager.stats.manager_verb_replays.inc();
            return .{ .granted = .{ .grants = grants, .already = already } };
        }
        // The structural door: a grant to any appender but the manager
        // cycles ring 0's window clear of the granted slots first.
        if (req.appender_id != journal.FIXED_RING_APPENDER) {
            switch (self.mgr.structuralDoor(picked.items, self.seams.ring0Tail(self.seams.ctx)) catch |e| return switch (e) {
                error.Io => error.Io,
                error.Corrupt => error.Corrupt,
            }) {
                .cleared => {},
                .deferred => return error.Deferred,
            }
        }
        // RAM first (g += 1), the words, then ONE control entry.
        var recs: std.ArrayList(manager.ControlRecord) = .empty;
        defer recs.deinit(gpa);
        var values = try gpa.alloc([cr.SlotStateWire.LEN]u8, picked.items.len);
        defer gpa.free(values);
        var keys = try gpa.alloc(cr.ControlKey, picked.items.len);
        defer gpa.free(keys);
        var minted: usize = 0;
        errdefer for (picked.items[0..minted]) |s| self.table.rollbackUnwritten(s);
        for (picked.items, 0..) |s, i| {
            const words = try self.grantWords(s);
            const rotor = req.rotor or i >= req.explicit.len;
            const g = switch (try self.table.acquire(s, req.appender_id, rotor)) {
                .granted => |g| g,
                // A racer (the recovery, a wire ask) took it under the mutex:
                // a protocol bug — the mutex serializes every grant.
                .refused => return error.Corrupt,
            };
            minted += 1;
            try self.table.setWords(s, words);
            grants[i] = .{ .slot = s, .g = g, .words = words };
            const st: cr.SlotState = .{ .variant = .leased, .appender_id = req.appender_id, .g = g, .root = words.root, .cursor = words.cursor, .slot_tree_extents = words.extents, .seq_floor = words.seq_floor };
            values[i] = st.encode();
            keys[i] = cr.slotStateKey(s);
            try recs.append(gpa, .{ .put = .{ .key = keys[i].slice(), .value = &values[i] } });
        }
        try self.control(recs.items);
        stats.slot_grants.add(grants.len);
        // The install: this process's regions adopt now; a wire grantee's
        // install is its own (the reply carries the words) and the tree
        // leaves THIS process's custody (`foreignGranted`).
        if (self.seams.ownsRegion(self.seams.ctx, req.appender_id)) {
            for (grants) |g| self.seams.install(self.seams.ctx, g.slot, req.appender_id, g.g, g.words, true) catch |e| {
                log.err("slot leases: the install of slot {d} after its grant to appender {d} failed ({s}) — tree 0 names the lease; the next open re-adopts it", .{ g.slot, req.appender_id, @errorName(e) });
            };
        } else if (self.seams.foreignGranted) |f| {
            for (grants) |g| f(self.seams.ctx, g.slot);
        }
        const t1 = clock.monotonicNs();
        self.mgr.ledger.record(t1, 0, t1 - t0, 0);
        return .{ .granted = .{ .grants = grants, .already = already } };
    }

    /// ONE slot for `requester`: an offer standing for it is THE ACCEPT —
    /// under the handover mutex the holder's flush-then-transfer, then the
    /// grant under the same hold (a door parked on the slot wakes only once
    /// the requester holds it); else a first touch (`Unleased`), else the
    /// refusal naming the holder.
    pub fn acquireSlot(self: *Verbs, gpa: Allocator, requester: AppenderId, slot: u16, admit: Admit) Error!AcquireOutcome {
        try self.requireLease();
        if (!screen.slot(slot).isOk()) return error.Rejected;
        const now = clock.monotonicNs();
        if (self.table.offeredTo(slot, now)) |to| if (to == requester) {
            const holder = self.table.view(slot).holder;
            self.handover_lock.lockUncancelable(self.io);
            defer self.handover_lock.unlock(self.io);
            // Re-read under the hold: a lapse or a racing accept took it.
            if (self.table.offeredTo(slot, clock.monotonicNs()) != requester) return self.acquireSlots(gpa, .{ .appender_id = requester, .explicit = &.{slot}, .admit = admit });
            try self.seams.transfer(self.seams.ctx, slot, holder, requester);
            const out = try self.acquireSlots(gpa, .{ .appender_id = requester, .explicit = &.{slot}, .admit = admit });
            self.door.wakeParkers();
            return out;
        };
        return self.acquireSlots(gpa, .{ .appender_id = requester, .explicit = &.{slot}, .admit = admit });
    }

    // ---- OfferSlot (T2) ----

    pub const OfferResult = enum { offered, busy };

    /// The holder names ONE requester; the stand ends at `expires_ns`. A
    /// second dominating ship before the accept is `busy` (legal).
    pub fn offerSlot(self: *Verbs, holder: AppenderId, slot: u16, to: AppenderId, expires_ns: u64) Error!OfferResult {
        try self.requireLease();
        // In-process the manager's own region (id 0) is a requester like
        // any other; the wire screen (`screen.offer`) refuses `to == 0`.
        if (!screen.slot(slot).isOk() or to == holder) return error.Rejected;
        return switch (try self.table.offer(slot, holder, to, expires_ns)) {
            .offered => blk: {
                if (self.offer_push) |op| if (!self.seams.ownsRegion(self.seams.ctx, to)) op.push(op.ctx, to, slot, holder);
                break :blk .offered;
            },
            .busy => .busy,
            .not_holder => error.Refused,
        };
    }

    // ---- ReleaseSlot (T5) ----

    pub const ReleaseReq = struct {
        appender_id: AppenderId,
        slot: u16,
        g: u32,
        words: SlotWords,
        tails: []const cr.TailEntry = &.{},
        /// The appender's page words the screen bounds the floor by
        /// (`page.seq_offset`, `head_hint`, the ring length); null = an
        /// in-process release whose words are the plane's own.
        page_bound: ?struct { seq_offset: u64, head_hint: u64, ring_len: u64 } = null,
    };

    pub const ReleaseResult = enum { released, already };

    /// Every word screened against durable state BEFORE any effect; the
    /// verdict against tree 0's record (a stale `g` is refused — the
    /// must-stay-0 witness class; the same `g` on an `Unleased` record is
    /// the replay); ONE control entry `Unleased` + `slot_tails`; ring 0's
    /// offset raised; the RAM drop LAST.
    pub fn releaseSlot(self: *Verbs, gpa: Allocator, req: ReleaseReq) Error!ReleaseResult {
        try self.requireLease();
        if (!screen.slot(req.slot).isOk()) return error.Rejected;
        self.mgr.verbs_lock.lockUncancelable(self.io);
        defer self.mgr.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        const cur = (try self.readState(req.slot)) orelse return error.Rejected;
        if (cur.variant == .unleased) {
            if (cur.g == req.g) {
                manager.stats.manager_verb_replays.inc();
                return .already;
            }
            manager.stats.manager_verb_refusals.fire("a ReleaseSlot named a generation tree 0 does not hold");
            return error.Refused;
        }
        if (cur.appender_id != req.appender_id or cur.g != req.g) {
            manager.stats.manager_verb_refusals.fire("a ReleaseSlot from a non-lessee or a stale generation");
            return error.Refused;
        }
        // The screen (spec §4.7): the seq floor strictly above the grant's
        // and under the derived bound; the cursor in range; the root the
        // recorded one or a node-aligned heap address.
        const g = self.mgr.vol.vt.geometry(self.mgr.vol.ctx);
        const bound: u64 = if (req.page_bound) |pb| @max(pb.seq_offset, cur.seq_floor + 1) + pb.head_hint + 2 * pb.ring_len else screen.SEQ_FRONTIER_SANE_MAX;
        const b: screen.SlotWordBounds = .{ .granted_floor = cur.seq_floor, .release_seq_floor_bound = bound, .recorded_cursor = cur.cursor, .recorded_root = .{ .addr = cur.root.addr, .seq = cur.root.seq }, .total_extents = g.total_extents, .heap = .{ .start = g.heap.start, .end = g.heap.end(), .node_size = @intCast(g.node_size) } };
        var tails_wire: std.ArrayList(verbs.LeafTailWire) = .empty;
        defer tails_wire.deinit(gpa);
        for (req.tails) |t| try tails_wire.append(gpa, .{ .leaf_addr = t.leaf_addr, .tail = t.tail, .reserved = 0 });
        const words_wire: verbs.SlotWordsWire = .{ .root = .{ .addr = req.words.root.addr, .seq = req.words.root.seq }, .cursor = req.words.cursor, .seq_floor = req.words.seq_floor, .slot_tree_extents = req.words.extents, .reserved = 0 };
        switch (screen.releaseWords(words_wire, tails_wire.items, &b)) {
            .ok => {},
            .rejected => |why| {
                log.warn("slot leases: ReleaseSlot of slot {d} by appender {d} rejected at the screen ({t})", .{ req.slot, req.appender_id, why });
                return error.Rejected;
            },
        }
        // ONE entry: Unleased + the tails stamped with the g they attest.
        const next: cr.SlotState = .{ .variant = .unleased, .g = req.g, .root = req.words.root, .cursor = @max(req.words.cursor, cur.cursor), .slot_tree_extents = req.words.extents, .last_written = self.table.lease_seq + 1, .seq_floor = @max(req.words.seq_floor, cur.seq_floor) };
        const value = next.encode();
        const value_cap = geometry.recordValueCap(g.node_size);
        const sk = cr.slotStateKey(req.slot);
        const tk = cr.slotTailsKey(req.slot);
        var recs: std.ArrayList(manager.ControlRecord) = .empty;
        defer recs.deinit(gpa);
        try recs.append(gpa, .{ .put = .{ .key = sk.slice(), .value = &value } });
        // The tails: inline under the value cap, else SPILLED into claimed
        // extents (`tails_spill`) named by the record and the entry's
        // `alloc` deltas; a spill the heap refuses lands the release
        // without its tails (the leaves screen by the lease alone, loud).
        var spilled: ?tails_spill.Spilled = null;
        defer if (spilled) |*sp| sp.deinit();
        const inline_rec: cr.SlotTails = .{ .inline_tails = .{ .g = req.g, .tails = req.tails } };
        var tails_rec: ?cr.SlotTails = inline_rec;
        if (cr.slotTailsEncodedLen(inline_rec) > value_cap) {
            if (tails_spill.spill(gpa, self.mgr.vol, req.tails)) |sp| {
                spilled = sp;
                tails_rec = sp.record(req.g);
            } else |e| switch (e) {
                error.NoSpace => {
                    log.warn("slot leases: slot {d}'s {d} leaf tails exceed the inline cap and the heap refused the spill — released WITHOUT its tails (its leaves screen by the lease alone)", .{ req.slot, req.tails.len });
                    tails_rec = null;
                },
                error.Io => return error.Io,
                error.OutOfMemory => return error.OutOfMemory,
            }
        }
        var tails_buf: []u8 = &.{};
        defer gpa.free(tails_buf);
        if (tails_rec) |tr| {
            tails_buf = try gpa.alloc(u8, cr.slotTailsEncodedLen(tr));
            const tails_enc = cr.encodeSlotTails(tr, tails_buf) catch return error.Corrupt;
            try recs.append(gpa, .{ .put = .{ .key = tk.slice(), .value = tails_enc } });
        }
        if (spilled) |sp| for (sp.extents) |e| try recs.append(gpa, .{ .alloc = e });
        // A spilled record this one supersedes gives its extents back
        // with the same entry (the frees land durable before the bits).
        var old_extents: []u64 = &.{};
        defer gpa.free(old_extents);
        if (try self.mgr.vol.vt.lookup(self.mgr.vol.ctx, gpa, tk.slice())) |old_raw| {
            defer gpa.free(old_raw);
            if (cr.decodeSlotTails(old_raw)) |old| {
                if (old.spilled) {
                    old_extents = try tails_spill.extentsOf(gpa, self.mgr.vol, old);
                    for (old_extents) |e| try recs.append(gpa, .{ .free = e });
                }
            } else |_| {}
        }
        self.control(recs.items) catch |e| {
            if (spilled) |sp| for (sp.extents) |x| self.mgr.vol.vt.releaseUnpublished(self.mgr.vol.ctx, x);
            return e;
        };
        for (old_extents) |e| self.mgr.vol.vt.releaseUnpublished(self.mgr.vol.ctx, e);
        // The frame screen's rule 2 reads the tails the entry just made
        // durable (the RAM index follows the durable word).
        if (tails_rec != null) try self.table.recordTails(req.slot, req.g, req.tails);
        self.seams.raiseRing0SeqFloor(self.seams.ctx, next.seq_floor);
        switch (try self.table.release(req.slot, req.appender_id, req.g, .{ .root = next.root, .cursor = next.cursor, .extents = next.slot_tree_extents, .seq_floor = next.seq_floor }, clock.monotonicNs())) {
            .released => {},
            .already => {},
            .refused => return error.Corrupt,
        }
        self.seams.released(self.seams.ctx, req.slot);
        stats.slot_releases.inc();
        self.door.wakeParkers();
        const t1 = clock.monotonicNs();
        self.mgr.ledger.record(t1, 0, t1 - t0, 0);
        return .released;
    }

    // ---- ResolveSlot ----

    pub fn resolveSlot(self: *Verbs, slot: u16) Error!Resolved {
        try self.requireLease();
        if (!screen.slot(slot).isOk()) return error.Rejected;
        stats.slot_resolve_rpcs.inc();
        const v = self.table.view(slot);
        return switch (v.state) {
            // A handover's released slot answers its accepter while the
            // stand holds: the asker's touch goes where the accepter's
            // first touch takes the slot (the accepter itself applies).
            .unleased => if (self.table.reservedFor(slot, clock.monotonicNs())) |to| .{ .holder = .{ .appender_id = to, .g = v.g, .dead = false } } else .{ .unleased = .{ .g = v.g } },
            .migrating => .{ .unleased = .{ .g = v.g } },
            .leased, .offered, .releasing => .{ .holder = .{ .appender_id = v.holder, .g = v.g, .dead = v.dead } },
        };
    }

    // ---- the wire handlers (the manager's HandlerTable) ----

    /// The plane the handlers recover from the manager.
    fn of(mgr: *manager.Manager) ?*Verbs {
        const p = mgr.slot_plane orelse return null;
        return @ptrCast(@alignCast(p));
    }

    pub fn bindHandlers(table: *manager.HandlerTable) void {
        table.bind(.acquire_slots, handleAcquireSlots);
        table.bind(.acquire_slot, handleAcquireSlot);
        table.bind(.offer_slot, handleOfferSlot);
        table.bind(.release_slot, handleReleaseSlot);
        table.bind(.resolve_slot, handleResolveSlot);
    }

    fn classify(e: Error) manager.Reply {
        const c = manager.classify(switch (e) {
            error.Rejected => error.Rejected,
            error.Refused => error.Refused,
            error.Deferred => error.Deferred,
            error.Busy, error.RotorAtCap => error.Refused,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
            error.OutOfMemory => error.OutOfMemory,
        }, null, null);
        return .{ .status = c.status, .reason = c.reason };
    }

    fn encodeAcquire(gpa: Allocator, out: AcquireOutcome) manager.Reply {
        var items: std.ArrayList(verbs.SlotGrantWire) = .empty;
        defer items.deinit(gpa);
        var head: verbs.AcquireHead = .{ .kind = 0, .reserved = 0, .n = 0, .already = 0, .slot = 0, .holder = 0, .g = 0 };
        switch (out) {
            .granted => |g| {
                defer gpa.free(g.grants);
                for (g.grants) |gr| items.append(gpa, .{ .words = .{ .root = .{ .addr = gr.words.root.addr, .seq = gr.words.root.seq }, .cursor = gr.words.cursor, .seq_floor = gr.words.seq_floor, .slot_tree_extents = gr.words.extents, .reserved = 0 }, .g = gr.g, .slot = gr.slot, .reserved = 0 }) catch return .{ .status = .refused, .reason = .io };
                head.n = @intCast(items.items.len);
                head.already = g.already;
            },
            .refused => |r| {
                head.kind = 1;
                head.slot = r.slot;
                head.holder = r.holder;
                head.g = r.g;
            },
        }
        const buf = gpa.alloc(u8, codec.wireSize(verbs.AcquireHead) + items.items.len * codec.wireSize(verbs.SlotGrantWire)) catch return .{ .status = .refused, .reason = .io };
        var b: codec.Builder = .init(buf);
        (verbs.AcquireReply{ .head = head, .items = items.items }).encode(&b) catch unreachable;
        return .{ .status = .ok, .payload = buf };
    }

    fn handleAcquireSlots(mgr: *manager.Manager, gpa: Allocator, payload: []const u8) manager.Reply {
        const self = of(mgr) orelse return .{ .status = .rejected, .reason = .verb_unserved };
        const req = verbs.decodeRequest(.acquire_slots, payload) catch return .{ .status = .malformed };
        const r = req.acquire_slots;
        if (!screen.acquireWant(r.want).isOk()) return .{ .status = .rejected, .reason = .screen_rejected };
        const out = self.acquireSlots(gpa, .{ .appender_id = r.appender_id, .want = r.want, .admit = .try_ }) catch |e| return classify(e);
        return encodeAcquire(gpa, out);
    }

    fn handleAcquireSlot(mgr: *manager.Manager, gpa: Allocator, payload: []const u8) manager.Reply {
        const self = of(mgr) orelse return .{ .status = .rejected, .reason = .verb_unserved };
        const req = verbs.decodeRequest(.acquire_slot, payload) catch return .{ .status = .malformed };
        const r = req.acquire_slot;
        const out = self.acquireSlot(gpa, r.appender_id, r.slot, .try_) catch |e| return classify(e);
        return encodeAcquire(gpa, out);
    }

    fn ack(gpa: Allocator, already: bool) manager.Reply {
        const buf = gpa.alloc(u8, codec.wireSize(verbs.Ack)) catch return .{ .status = .refused, .reason = .io };
        var b: codec.Builder = .init(buf);
        b.put(verbs.Ack, if (already) .replay else .fresh) catch unreachable;
        return .{ .status = .ok, .payload = buf };
    }

    fn handleOfferSlot(mgr: *manager.Manager, gpa: Allocator, payload: []const u8) manager.Reply {
        const self = of(mgr) orelse return .{ .status = .rejected, .reason = .verb_unserved };
        const req = verbs.decodeRequest(.offer_slot, payload) catch return .{ .status = .malformed };
        const r = req.offer_slot;
        if (!screen.offer(r).isOk()) return .{ .status = .rejected, .reason = .screen_rejected };
        const expires = clock.monotonicNs() + self.offerStandNs(true);
        const out = self.offerSlot(r.appender_id, r.slot, r.to, expires) catch |e| return classify(e);
        return ack(gpa, out == .busy);
    }

    fn handleReleaseSlot(mgr: *manager.Manager, gpa: Allocator, payload: []const u8) manager.Reply {
        const self = of(mgr) orelse return .{ .status = .rejected, .reason = .verb_unserved };
        const req = verbs.decodeRequest(.release_slot, payload) catch return .{ .status = .malformed };
        const r = req.release_slot;
        // The frame's appender must name a Live page (never one of this
        // mount's own regions on the wire).
        if (self.seams.ownsRegion(self.seams.ctx, r.head.appender_id)) return .{ .status = .rejected, .reason = .screen_rejected };
        const p = (self.mgr.readPageOf(r.head.appender_id) catch return .{ .status = .refused, .reason = .io }) orelse return .{ .status = .rejected, .reason = .screen_rejected };
        if (p.state != .live) return .{ .status = .rejected, .reason = .screen_rejected };
        var tails: std.ArrayList(cr.TailEntry) = .empty;
        defer tails.deinit(gpa);
        for (r.items) |t| tails.append(gpa, .{ .leaf_addr = t.leaf_addr, .tail = t.tail }) catch return .{ .status = .refused, .reason = .io };
        const out = self.releaseSlot(gpa, .{
            .appender_id = r.head.appender_id,
            .slot = r.head.slot,
            .g = r.head.g,
            .words = .{ .root = .{ .addr = r.head.words.root.addr, .seq = r.head.words.root.seq }, .cursor = r.head.words.cursor, .extents = r.head.words.slot_tree_extents, .seq_floor = r.head.words.seq_floor },
            .tails = tails.items,
            .page_bound = .{ .seq_offset = p.seq_offset, .head_hint = p.head_hint, .ring_len = p.ringBytes() },
        }) catch |e| return classify(e);
        return ack(gpa, out == .already);
    }

    fn handleResolveSlot(mgr: *manager.Manager, gpa: Allocator, payload: []const u8) manager.Reply {
        const self = of(mgr) orelse return .{ .status = .rejected, .reason = .verb_unserved };
        const req = verbs.decodeRequest(.resolve_slot, payload) catch return .{ .status = .malformed };
        const out = self.resolveSlot(req.resolve_slot.slot) catch |e| return classify(e);
        const buf = gpa.alloc(u8, codec.wireSize(verbs.SlotHolderReply)) catch return .{ .status = .refused, .reason = .io };
        var b: codec.Builder = .init(buf);
        const rep: verbs.SlotHolderReply = switch (out) {
            .unleased => |u| .{ .kind = 1, .reserved = @splat(0), .appender_id = 0, .g = u.g, .reserved2 = 0 },
            .holder => |h| .{ .kind = 0, .reserved = @splat(0), .appender_id = h.appender_id, .g = h.g, .reserved2 = 0 },
        };
        b.put(verbs.SlotHolderReply, rep) catch unreachable;
        return .{ .status = .ok, .payload = buf };
    }

    /// The offer's stand (spec §4.3): in-process `clamp(min(beat, T_idle/3),
    /// 1 ms)`; on the wire `beat + handover_bound` (the recall is a PUSH
    /// verb, spec §9 Q10 — the renewal carriage is the belt).
    pub fn offerStandNs(self: *const Verbs, wire: bool) u64 {
        const beat = HEARTBEAT_NS;
        const t_idle = self.door.t_idle_ns;
        if (!wire) return @max(@min(beat, t_idle / 3), std.time.ns_per_ms);
        return beat + HANDOVER_BOUND_FLOOR_NS;
    }
};

/// The renewal BEAT: `claim.HEARTBEAT_INTERVAL_S` (10 s).
pub const HEARTBEAT_NS: u64 = 10 * std.time.ns_per_s;
/// A wire stand's handover term before any measurement: two of the
/// measured upper handover (2 × 20 ms, spec §4.4's idle-arm band).
pub const HANDOVER_BOUND_FLOOR_NS: u64 = 40 * std.time.ns_per_ms;

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "derivations tie: M at 1 / 512 / 1 024 / 12 500 / 2^20 writers; the explicit knob wins verbatim; the rotor cap is 2M; A_max = max(bytes/64, node) and the static ceiling refuses outside [node, heap]; cursors fold as floors" {
    try testing.expectEqual(@as(u64, 64), mintSlotsDerived(1));
    try testing.expectEqual(@as(u64, 64), mintSlotsDerived(0));
    try testing.expectEqual(@as(u64, 64), mintSlotsDerived(512));
    try testing.expectEqual(@as(u64, 32), mintSlotsDerived(1024));
    try testing.expectEqual(@as(u64, 2), mintSlotsDerived(12_500));
    try testing.expectEqual(@as(u64, 1), mintSlotsDerived(1 << 20));
    try testing.expectEqual(@as(u64, 7), resolveMintSlots(7, 1));
    try testing.expectEqual(@as(u64, 128), rotorCap(64));
    const node: u64 = 256 * derive.KiB;
    try testing.expectEqual(node, affinityCeiling(0, node));
    try testing.expectEqual(@as(u64, 2 * derive.MiB), affinityCeiling(128 * derive.MiB, node));
    try testing.expectEqual(@as(u64, 4 * derive.MiB), try resolveAffinityCeiling(4, 0, node, 1 << 30));
    try testing.expectError(error.BelowNodeSize, resolveAffinityCeiling(1, 0, 2 * derive.MiB, 1 << 30));
    try testing.expectError(error.AboveHeap, resolveAffinityCeiling(2048, 0, node, 1 << 30));
    try testing.expectEqual(@as(u64, 2), cursorFloor(0, null, null, null));
    try testing.expectEqual(@as(u64, 50), cursorFloor(10, 50, 20, 30));
    try testing.expectEqual(@as(u64, 101), cursorFloor(10, 50, 100, 30));
    try testing.expectEqual(@as(u64, 201), cursorFloor(10, 50, 100, 200));
}

test "mint_choice: affinity below the ceiling; a spill to the most-headroom rotor at/over it; every rotor strictly over by one extent overflows under 2M and takes the smallest at 2M; exact equality is not over; the least-over tree wins once all are over; the counters close" {
    const node: u64 = 1024;
    const a_max: u64 = 10 * node;
    const rotor = [_]RotorTree{ .{ .slot = 5, .bytes = 3 * node }, .{ .slot = 6, .bytes = 8 * node }, .{ .slot = 7, .bytes = 12 * node } };
    const base: MintInputs = .{ .parent_slot = 9, .parent_leased_non_native = true, .parent_tree_bytes = 2 * node, .a_max = a_max, .node_size = node, .rotor = &rotor, .rotor_cap = 4, .rr = 0 };
    try testing.expectEqual(@as(u16, 9), mintChoice(base).affinity);
    var over = base;
    over.parent_tree_bytes = a_max;
    try testing.expectEqual(@as(u16, 5), mintChoice(over).rotor);
    over.parent_leased_non_native = false;
    try testing.expectEqual(@as(u16, 5), mintChoice(over).rotor);
    // All strictly over (≥ a_max + node): overflow under the cap.
    const all_over = [_]RotorTree{ .{ .slot = 5, .bytes = 11 * node }, .{ .slot = 6, .bytes = 13 * node } };
    var o = over;
    o.rotor = &all_over;
    try testing.expectEqual(MintChoice.overflow, mintChoice(o));
    o.rotor_cap = 2;
    try testing.expectEqual(@as(u16, 5), mintChoice(o).smallest);
    // Exact equality is NOT over: the headroom arm picks the least over.
    const at_cap = [_]RotorTree{ .{ .slot = 5, .bytes = 11 * node }, .{ .slot = 6, .bytes = 10 * node } };
    o.rotor = &at_cap;
    try testing.expectEqual(@as(u16, 6), mintChoice(o).rotor);
    // Signed headroom: every tree over by ≥ 1 but the cap at 2M ⇒ smallest;
    // under the cap but one tree exactly at a_max ⇒ that one.
    o.rotor_cap = 4;
    try testing.expectEqual(@as(u16, 6), mintChoice(o).rotor);
    var none = over;
    none.rotor = &.{};
    try testing.expectEqual(MintChoice.none, mintChoice(none));
    const a0 = stats.affinity_mints.load();
    const r0 = stats.rotor_mints.load();
    const s0 = stats.affinity_ceiling_spills.load();
    countChoice(.{ .affinity = 9 }, true);
    countChoice(.{ .rotor = 5 }, true);
    countChoice(.{ .smallest = 5 }, false);
    countChoice(.overflow, true);
    try testing.expectEqual(a0 + 1, stats.affinity_mints.load());
    try testing.expectEqual(r0 + 2, stats.rotor_mints.load());
    try testing.expectEqual(s0 + 1, stats.affinity_ceiling_spills.load());
}

test "gather: the opt-in vocabulary; the arm answers the parent's slot only on this volume, non-native, opted in; counted" {
    try testing.expect(gatherOptIn("1") and gatherOptIn(" yes\n") and gatherOptIn("ON") and gatherOptIn("True"));
    try testing.expect(!gatherOptIn("0") and !gatherOptIn("") and !gatherOptIn("maybe"));
    const g0 = stats.dir_gather_mints.load();
    try testing.expectEqual(@as(?u16, null), gatherMintSlot(false, false, true, 4));
    try testing.expectEqual(@as(?u16, null), gatherMintSlot(true, true, true, 4));
    try testing.expectEqual(@as(?u16, null), gatherMintSlot(true, false, false, 4));
    try testing.expectEqual(@as(?u16, 4), gatherMintSlot(true, false, true, 4));
    try testing.expectEqual(g0 + 1, stats.dir_gather_mints.load());
}

test "the cadence's decisions: forced shrink releases idle rotor slots smallest-first down to M (64 → 32 past 512 writers); LRU releases idle non-rotor non-native slots ascending past the budget; a region releases only when empty, drained and quiet" {
    var held: [64]HeldSlot = undefined;
    for (&held, 0..) |*h, i| h.* = .{ .slot = @intCast(100 + i), .idle = i % 2 == 0, .rotor = true, .native = false, .extents = @intCast(64 - i) };
    var out: [64]u16 = undefined;
    const v = forcedShrinkVictims(&held, mintSlotsDerived(1024), &out);
    // 32 idle rotor slots, smallest extents first: slot 162 (extents 2) first.
    try testing.expectEqual(@as(usize, 32), v.len);
    try testing.expectEqual(@as(u16, 162), v[0]);
    try testing.expectEqual(@as(usize, 0), forcedShrinkVictims(&held, 64, &out).len);
    var mixed = [_]HeldSlot{ .{ .slot = 3, .idle = true, .rotor = false, .native = false, .extents = 1 }, .{ .slot = 1, .idle = true, .rotor = false, .native = false, .extents = 1 }, .{ .slot = 2, .idle = false, .rotor = false, .native = false, .extents = 1 }, .{ .slot = 0, .idle = true, .rotor = false, .native = true, .extents = 1 } };
    try testing.expectEqualSlices(u16, &.{1}, lruReleaseVictims(&mixed, 3, &out));
    try testing.expectEqualSlices(u16, &.{ 1, 3 }, lruReleaseVictims(&mixed, 2, &out));
    try testing.expectEqual(@as(usize, 0), lruReleaseVictims(&mixed, 4, &out).len);
    try testing.expect(regionReleasable(3, 0, 100, 100, false, 0));
    try testing.expect(!regionReleasable(0, 0, 100, 100, false, 0));
    try testing.expect(!regionReleasable(3, 1, 100, 100, false, 0));
    try testing.expect(!regionReleasable(3, 0, 101, 100, false, 0));
    try testing.expect(!regionReleasable(3, 0, 100, 100, true, 0));
    try testing.expect(!regionReleasable(3, 0, 100, 100, false, 1));
}

// ---- the verbs over the manager's RAM volume ----

pub const TestPlane = struct {
    gpa: Allocator,
    tv: *manager.TestVolume,
    mgr: manager.Manager,
    table: *LeaseTable,
    door: Door,
    verbs: Verbs,
    installs: u32 = 0,
    releases: u32 = 0,
    transfers: u32 = 0,
    parks: u32 = 0,
    ring0_floor: u64 = 0,
    own: [4]AppenderId = .{ 0, 1, 0, 0 },
    own_n: usize = 1,
    candidates: []const u16 = &.{ 1, 2, 3, 4, 5, 6, 7, 8 },

    pub fn init(gpa: Allocator) !*TestPlane {
        const self = try gpa.create(TestPlane);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .tv = try manager.TestVolume.init(gpa, 64 << 20, 65536, 4096), .mgr = undefined, .table = undefined, .door = undefined, .verbs = undefined };
        errdefer self.tv.deinit();
        try self.tv.liveManagerPage();
        self.mgr = manager.Manager.init(gpa, testing.io, self.tv.volume());
        self.mgr.holdLease(1);
        self.table = try LeaseTable.init(gpa);
        self.door = .{ .table = self.table, .io = testing.io, .armed = true, .first_touch = .{ .ctx = self, .acquire = firstTouch }, .t_idle_ns = 1 << 40 };
        self.verbs = Verbs.init(gpa, testing.io, &self.mgr, self.table, &self.door, .{
            .ctx = self,
            .rotorCandidates = cands,
            .nativeSlot = native,
            .ownsRegion = ownsFn,
            .ring0Tail = tail,
            .raiseRing0SeqFloor = raise,
            .install = install,
            .released = released,
            .transfer = transfer,
            .parkForRoom = park,
        }, 4, 65536);
        self.mgr.slot_plane = &self.verbs;
        return self;
    }

    pub fn deinit(self: *TestPlane) void {
        self.table.deinit();
        self.mgr.deinit();
        self.tv.deinit();
        self.gpa.destroy(self);
    }

    fn self_(ctx: *anyopaque) *TestPlane {
        return @ptrCast(@alignCast(ctx));
    }

    fn ownsFn(ctx: *anyopaque, id: AppenderId) bool {
        const s = self_(ctx);
        for (s.own[0..s.own_n]) |o| if (o == id) return true;
        return false;
    }

    fn cands(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]u16 {
        return gpa.dupe(u16, self_(ctx).candidates);
    }

    fn native(_: *anyopaque) ?u16 {
        return 0;
    }

    fn tail(ctx: *anyopaque) u64 {
        return self_(ctx).tv.ring0_head;
    }

    fn raise(ctx: *anyopaque, floor: u64) void {
        self_(ctx).ring0_floor = @max(self_(ctx).ring0_floor, floor);
    }

    fn install(ctx: *anyopaque, _: u16, _: AppenderId, _: u32, _: SlotWords, _: bool) Error!void {
        self_(ctx).installs += 1;
    }

    fn released(ctx: *anyopaque, _: u16) void {
        self_(ctx).releases += 1;
    }

    /// The in-process transfer: the holder releases with its words.
    fn transfer(ctx: *anyopaque, slot: u16, holder: AppenderId, _: AppenderId) Error!void {
        const s = self_(ctx);
        s.transfers += 1;
        const g = s.door.beginRelease(slot, holder, false) orelse return error.Busy;
        _ = s.door.drain(slot, std.math.maxInt(u64));
        const words = s.table.wordsOf(slot);
        _ = try s.verbs.releaseSlot(s.gpa, .{ .appender_id = holder, .slot = slot, .g = g, .words = .{ .root = words.root, .cursor = words.cursor + 5, .extents = words.extents, .seq_floor = words.seq_floor + 1 } });
    }

    fn park(ctx: *anyopaque, _: u64) Error!void {
        self_(ctx).parks += 1;
    }

    fn firstTouch(ctx: *anyopaque, slot: u16, requester: AppenderId) door_mod.Error!void {
        const s = self_(ctx);
        const out = s.verbs.acquireSlot(s.gpa, requester, slot, .park) catch |e| return switch (e) {
            error.Deferred => error.Deferred,
            error.Busy, error.RotorAtCap, error.Refused, error.Rejected => error.SlotBusy,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
            error.OutOfMemory => error.OutOfMemory,
        };
        switch (out) {
            .granted => |g| s.gpa.free(g.grants),
            .refused => return error.SlotBusy,
        }
    }

    pub fn state(self: *TestPlane, slot: u16) ?cr.SlotState {
        const raw = self.tv.get(cr.slotStateKey(slot).slice()) orelse return null;
        return cr.SlotStateWire.decode(raw) catch unreachable;
    }
};

test "AcquireSlots: a rotor ask grants unleased-then-idle candidates in ONE barriered control entry with Leased{g=1} records, installs own regions, replays an explicit held slot as already, refuses one another appender holds naming it, and refuses a rotor ask past 2M; the parking admission is taken for a Park ask; a failed write rolls the RAM back keeping g" {
    const gpa = testing.allocator;
    const p = try TestPlane.init(gpa);
    defer p.deinit();
    const entries0 = p.tv.control_entries;
    const out = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .want = 3, .admit = .park });
    defer gpa.free(out.granted.grants);
    try testing.expectEqual(@as(usize, 3), out.granted.grants.len);
    try testing.expectEqualSlices(u16, &.{ 1, 2, 3 }, &.{ out.granted.grants[0].slot, out.granted.grants[1].slot, out.granted.grants[2].slot });
    try testing.expectEqual(entries0 + 1, p.tv.control_entries);
    try testing.expectEqual(@as(u32, 1), p.parks);
    try testing.expectEqual(@as(u32, 3), p.installs);
    const s1 = p.state(1).?;
    try testing.expect(s1.variant == .leased and s1.appender_id == 0 and s1.g == 1);
    try testing.expect(p.table.view(1).leasedBy(0) and p.table.view(1).rotor);
    // Explicit: 1 again by 0 is a replay; by 2 a refusal naming 0; 4 fresh.
    const rep = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{1} });
    defer gpa.free(rep.granted.grants);
    try testing.expectEqual(@as(u16, 1), rep.granted.already);
    const ref = try p.verbs.acquireSlots(gpa, .{ .appender_id = 2, .explicit = &.{1} });
    try testing.expectEqual(@as(u32, 0), ref.refused.holder);
    try testing.expectEqual(@as(u16, 1), ref.refused.slot);
    // The rotor cap (M = 4 ⇒ 2M = 8): 3 held + 6 wanted refuses.
    try testing.expectError(error.RotorAtCap, p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .want = 6 }));
    // A full window: deferred + kicked, the RAM rolled back with g kept.
    p.tv.window_full_next = 1;
    try testing.expectError(error.Deferred, p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{7} }));
    try testing.expectEqual(@as(u32, 1), p.tv.kicks);
    try testing.expectEqual(lt.State.unleased, p.table.view(7).state);
    try testing.expectEqual(@as(u32, 1), p.table.gOf(7));
    try testing.expect(p.state(7) == null);
    // The next grant of 7 carries g = 2 (one ahead of the rollback).
    const g7 = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{7} });
    defer gpa.free(g7.granted.grants);
    try testing.expectEqual(@as(u32, 2), g7.granted.grants[0].g);
    try testing.expectEqual(@as(u32, 2), p.state(7).?.g);
}

test "a grant to a non-manager appender takes the structural door: a window naming the slot is cycled clear within the bound; a moving tail at the bound defers with nothing written" {
    const gpa = testing.allocator;
    const p = try TestPlane.init(gpa);
    defer p.deinit();
    p.own_n = 2;
    try p.tv.window_names.append(gpa, 5);
    p.tv.cycles_until_clear = 3;
    const out = try p.verbs.acquireSlots(gpa, .{ .appender_id = 1, .explicit = &.{5} });
    defer gpa.free(out.granted.grants);
    try testing.expectEqual(@as(u32, 3), p.tv.cycles_run);
    try testing.expect(p.table.view(5).leasedBy(1));
    try p.tv.window_names.append(gpa, 6);
    p.tv.cycles_until_clear = 1000;
    const d0 = manager.stats.slot_grant_deferrals.load();
    try testing.expectError(error.Deferred, p.verbs.acquireSlots(gpa, .{ .appender_id = 1, .explicit = &.{6} }));
    try testing.expectEqual(d0 + 1, manager.stats.slot_grant_deferrals.load());
    try testing.expect(p.state(6) == null);
    try testing.expectEqual(lt.State.unleased, p.table.view(6).state);
}

test "ReleaseSlot: the words are screened before any effect (a floor at the grant's, a cursor below the record, a root outside the heap each reject with the volume unchanged); a stale g is the refusal tripwire; the live release writes Unleased + slot_tails in ONE entry, raises ring 0's floor, drops the RAM lease and the next grant carries the words at g + 1; a replay is already" {
    const gpa = testing.allocator;
    const p = try TestPlane.init(gpa);
    defer p.deinit();
    const g = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{3} });
    gpa.free(g.granted.grants);
    const in_heap = p.tv.g.heap.start + 7 * p.tv.g.node_size;
    const ok: SlotWords = .{ .root = .{ .addr = in_heap, .seq = 5 }, .cursor = 40, .extents = 2, .seq_floor = 10 };
    const entries0 = p.tv.control_entries;
    var bad = ok;
    bad.seq_floor = 0;
    try testing.expectError(error.Rejected, p.verbs.releaseSlot(gpa, .{ .appender_id = 0, .slot = 3, .g = 1, .words = bad }));
    bad = ok;
    bad.cursor = 1;
    try testing.expectError(error.Rejected, p.verbs.releaseSlot(gpa, .{ .appender_id = 0, .slot = 3, .g = 1, .words = bad }));
    bad = ok;
    bad.root.addr = 12;
    try testing.expectError(error.Rejected, p.verbs.releaseSlot(gpa, .{ .appender_id = 0, .slot = 3, .g = 1, .words = bad }));
    try testing.expectEqual(entries0, p.tv.control_entries);
    try testing.expect(p.table.view(3).leasedBy(0));
    const tw0 = manager.stats.manager_verb_refusals.load();
    manager.stats.manager_verb_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Refused, p.verbs.releaseSlot(gpa, .{ .appender_id = 0, .slot = 3, .g = 7, .words = ok }));
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 1, manager.stats.manager_verb_refusals.load());
    const tails = [_]cr.TailEntry{ .{ .leaf_addr = in_heap, .tail = 4096 }, .{ .leaf_addr = in_heap + 65536, .tail = 8192 } };
    try testing.expectEqual(Verbs.ReleaseResult.released, try p.verbs.releaseSlot(gpa, .{ .appender_id = 0, .slot = 3, .g = 1, .words = ok, .tails = &tails }));
    try testing.expectEqual(entries0 + 1, p.tv.control_entries);
    const s = p.state(3).?;
    try testing.expect(s.variant == .unleased and s.g == 1 and s.cursor == 40 and s.seq_floor == 10 and s.last_written == 1);
    const dt = cr.decodeSlotTails(p.tv.get(cr.slotTailsKey(3).slice()).?) catch unreachable;
    try testing.expectEqual(@as(u32, 1), dt.g);
    try testing.expectEqual(@as(u64, 10), p.ring0_floor);
    try testing.expectEqual(@as(u32, 1), p.releases);
    try testing.expectEqual(lt.State.unleased, p.table.view(3).state);
    try testing.expectEqual(Verbs.ReleaseResult.already, try p.verbs.releaseSlot(gpa, .{ .appender_id = 0, .slot = 3, .g = 1, .words = ok }));
    const g2 = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{3} });
    defer gpa.free(g2.granted.grants);
    try testing.expectEqual(@as(u32, 2), g2.granted.grants[0].g);
    try testing.expectEqual(@as(u64, 40), g2.granted.grants[0].words.cursor);
    try testing.expectEqual(@as(u64, 10), g2.granted.grants[0].words.seq_floor);
}

test "ReleaseSlot past the inline cap SPILLS (rung 10 item 5b): 1,100 tails at the 64 KiB node (cap 1,039) land in one claimed extent named by a spilled record with the extent's alloc delta in the SAME entry; the RAM index holds every tail; the next release of the slot with a small set goes inline and gives the spill extent back (the free delta, the bit)" {
    const gpa = testing.allocator;
    const p = try TestPlane.init(gpa);
    defer p.deinit();
    const g = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{5} });
    gpa.free(g.granted.grants);
    const in_heap = p.tv.g.heap.start + 7 * p.tv.g.node_size;
    const ok: SlotWords = .{ .root = .{ .addr = in_heap, .seq = 5 }, .cursor = 40, .extents = 2, .seq_floor = 10 };
    const cap = cr.slotTailsInlineCap(geometry.recordValueCap(p.tv.g.node_size));
    try testing.expectEqual(@as(u32, 1039), cap);
    const n: usize = 1100;
    const tails = try gpa.alloc(cr.TailEntry, n);
    defer gpa.free(tails);
    for (tails, 0..) |*t, i| t.* = .{ .leaf_addr = p.tv.g.heap.start + (i + 8) * p.tv.g.node_size, .tail = @intCast(4096 + i) };
    const claimed0 = p.tv.allocatedExtents();
    const entries0 = p.tv.control_entries;
    const spills0 = tails_spill.stats.slot_tails_spills.load();
    try testing.expectEqual(Verbs.ReleaseResult.released, try p.verbs.releaseSlot(gpa, .{ .appender_id = 0, .slot = 5, .g = 1, .words = ok, .tails = tails }));
    try testing.expectEqual(entries0 + 1, p.tv.control_entries);
    try testing.expectEqual(claimed0 + 1, p.tv.allocatedExtents());
    try testing.expectEqual(spills0 + 1, tails_spill.stats.slot_tails_spills.load());
    const dt = cr.decodeSlotTails(p.tv.get(cr.slotTailsKey(5).slice()).?) catch unreachable;
    try testing.expect(dt.spilled and dt.count == 1 and dt.g == 1);
    try testing.expectEqual(@as(u32, n), dt.spillRunAt(0).count);
    // The RAM index (the frame screen's rule 2) holds the first and the last.
    try testing.expectEqual(@as(u32, 4096), p.table.recordedTail(5, tails[0].leaf_addr).?.tail);
    try testing.expectEqual(@as(u32, 4096 + n - 1), p.table.recordedTail(5, tails[n - 1].leaf_addr).?.tail);
    // The spilled images read back whole through the volume's reader.
    var back: std.ArrayList(cr.TailEntry) = .empty;
    defer back.deinit(gpa);
    try testing.expect(try tails_spill.read(gpa, p.tv.volume().reader(), dt, &back));
    try testing.expectEqual(n, back.items.len);
    // Re-leased, then released with two tails: inline, the spill extent back.
    const g2 = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{5} });
    gpa.free(g2.granted.grants);
    const small = [_]cr.TailEntry{ .{ .leaf_addr = in_heap, .tail = 1 }, .{ .leaf_addr = in_heap + p.tv.g.node_size, .tail = 2 } };
    try testing.expectEqual(Verbs.ReleaseResult.released, try p.verbs.releaseSlot(gpa, .{ .appender_id = 0, .slot = 5, .g = 2, .words = .{ .root = ok.root, .cursor = 41, .extents = 2, .seq_floor = 11 }, .tails = &small }));
    try testing.expectEqual(claimed0, p.tv.allocatedExtents());
    const dt2 = cr.decodeSlotTails(p.tv.get(cr.slotTailsKey(5).slice()).?) catch unreachable;
    try testing.expect(!dt2.spilled and dt2.count == 2 and dt2.g == 2);
}

test "the in-process accept: an offer standing for the requester runs the holder's transfer then the grant under one hold; a lapsed offer is an ordinary refusal; ResolveSlot answers the table; the door's first touch acquires through the verb and the handlers round-trip the wire codecs through the manager's table" {
    const gpa = testing.allocator;
    const p = try TestPlane.init(gpa);
    defer p.deinit();
    p.own_n = 2;
    const g = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{4} });
    gpa.free(g.granted.grants);
    try testing.expectEqual(Verbs.OfferResult.offered, try p.verbs.offerSlot(0, 4, 1, std.math.maxInt(u64)));
    try testing.expectEqual(Verbs.OfferResult.busy, try p.verbs.offerSlot(0, 4, 1, std.math.maxInt(u64)));
    try testing.expectError(error.Refused, p.verbs.offerSlot(1, 4, 0, std.math.maxInt(u64)));
    try testing.expectError(error.Rejected, p.verbs.offerSlot(0, 4, 0, std.math.maxInt(u64)));
    const acc = try p.verbs.acquireSlot(gpa, 1, 4, .try_);
    defer gpa.free(acc.granted.grants);
    try testing.expectEqual(@as(u32, 1), p.transfers);
    try testing.expectEqual(@as(u32, 2), acc.granted.grants[0].g);
    try testing.expect(p.table.view(4).leasedBy(1));
    try testing.expect(p.state(4).?.appender_id == 1);
    // The requester's grant carries the release's words (cursor + 5).
    try testing.expectEqual(@as(u64, 7), acc.granted.grants[0].words.cursor);
    // A lapsed offer: ordinary refusal.
    try testing.expectEqual(Verbs.OfferResult.offered, try p.verbs.offerSlot(1, 4, 0, 1));
    const ref = try p.verbs.acquireSlot(gpa, 0, 4, .try_);
    try testing.expectEqual(@as(u32, 1), ref.refused.holder);
    var lapsed: [2]lt.LapsedOffer = undefined;
    try testing.expectEqual(@as(usize, 1), p.table.expireOffers(clock.monotonicNs(), &lapsed));
    const r = try p.verbs.resolveSlot(4);
    try testing.expectEqual(@as(u32, 1), r.holder.appender_id);
    try testing.expectEqual(@as(u32, 0), (try p.verbs.resolveSlot(8)).unleased.g);
    // The door's first touch of 8 by region 0 goes through acquireSlot.
    var tok: door_mod.Tokens = .{};
    try p.door.enter(&.{8}, 0, &tok);
    try testing.expect(p.table.view(8).leasedBy(0));
    try testing.expect(p.state(8).?.appender_id == 0);
    p.door.exit(&tok);
    // The handlers: AcquireSlot(2, 8) refused naming 0; ResolveSlot(8).
    var table: manager.HandlerTable = .{};
    Verbs.bindHandlers(&table);
    try testing.expect(table.served(.acquire_slot) and table.served(.release_slot));
    var buf: [256]u8 = undefined;
    var b: codec.Builder = .init(&buf);
    try verbs.encodeRequest(.{ .acquire_slot = .{ .volume = 0, .slot = 8, .appender_id = 2 } }, &b);
    const rep = table.dispatch(&p.mgr, gpa, .acquire_slot, b.written());
    defer gpa.free(rep.payload);
    try testing.expectEqual(manager.Status.ok, rep.status);
    const dec = try verbs.decodeReply(.acquire_slot, rep.payload);
    try testing.expectEqual(@as(u8, 1), dec.acquire_slot.head.kind);
    try testing.expectEqual(@as(u32, 0), dec.acquire_slot.head.holder);
    b = .init(&buf);
    try verbs.encodeRequest(.{ .resolve_slot = .{ .volume = 0, .slot = 8, .reserved = 0 } }, &b);
    const rr = table.dispatch(&p.mgr, gpa, .resolve_slot, b.written());
    defer gpa.free(rr.payload);
    const rd = try verbs.decodeReply(.resolve_slot, rr.payload);
    try testing.expectEqual(@as(u8, 0), rd.resolve_slot.kind);
    try testing.expectEqual(@as(u32, 0), rd.resolve_slot.appender_id);
    // A wire AcquireSlots past the cap rejects at the screen; a malformed
    // frame is malformed.
    b = .init(&buf);
    try verbs.encodeRequest(.{ .acquire_slots = .{ .volume = 0, .want = 129, .appender_id = 2 } }, &b);
    try testing.expectEqual(manager.Status.rejected, table.dispatch(&p.mgr, gpa, .acquire_slots, b.written()).status);
    try testing.expectEqual(manager.Status.malformed, table.dispatch(&p.mgr, gpa, .offer_slot, "xx").status);
}

test "the release → grant gap of an accept: the released slot stays RESERVED for its accepter while the offer's stand holds — a first touch by the holder (its served ship of the requester) or any third appender is refused naming the accepter and counted, the rotor pick skips it, the accepter's grant lands; a lapsed stand frees it" {
    const gpa = testing.allocator;
    const p = try TestPlane.init(gpa);
    defer p.deinit();
    p.own_n = 3;
    const g = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{4} });
    gpa.free(g.granted.grants);
    // The offer stands for 1; the holder's own transfer (the accept's first
    // half) releases the slot with the stand kept.
    try testing.expectEqual(Verbs.OfferResult.offered, try p.verbs.offerSlot(0, 4, 1, std.math.maxInt(u64)));
    try TestPlane.transfer(p, 4, 0, 1);
    try testing.expectEqual(lt.State.unleased, p.table.view(4).state);
    try testing.expectEqual(@as(?AppenderId, 1), p.table.reservedFor(4, clock.monotonicNs()));
    // `ResolveSlot` answers the accepter: every asker's touch goes where
    // the accepter's own first touch takes the slot.
    try testing.expectEqual(@as(AppenderId, 1), (try p.verbs.resolveSlot(4)).holder.appender_id);
    // The holder's first touch (a served ship of 1 landing in the gap) and
    // a third appender's are refused naming the accepter.
    const refusals0 = stats.slot_acquire_reserved_refusals.load();
    const r0 = try p.verbs.acquireSlot(gpa, 0, 4, .try_);
    try testing.expectEqual(@as(u32, 1), r0.refused.holder);
    const r2 = try p.verbs.acquireSlots(gpa, .{ .appender_id = 2, .explicit = &.{4} });
    try testing.expectEqual(@as(u32, 1), r2.refused.holder);
    try testing.expectEqual(refusals0 + 2, stats.slot_acquire_reserved_refusals.load());
    // The rotor pick never hands the reserved slot out.
    var picked: [4]u16 = undefined;
    for (p.table.pickUnleased(&.{ 4, 5, 6 }, picked[0..2])) |s| try testing.expect(s != 4);
    // The accepter's grant lands and clears the reservation.
    const acc = try p.verbs.acquireSlots(gpa, .{ .appender_id = 1, .explicit = &.{4} });
    defer gpa.free(acc.granted.grants);
    try testing.expect(p.table.view(4).leasedBy(1));
    try testing.expectEqual(@as(?AppenderId, null), p.table.reservedFor(4, clock.monotonicNs()));
    // A lapsed stand: the released slot is anyone's.
    const g5 = try p.verbs.acquireSlots(gpa, .{ .appender_id = 0, .explicit = &.{5} });
    gpa.free(g5.granted.grants);
    try testing.expectEqual(Verbs.OfferResult.offered, try p.verbs.offerSlot(0, 5, 1, clock.monotonicNs() + 1));
    try TestPlane.transfer(p, 5, 0, 1);
    try testing.expectEqual(@as(?AppenderId, null), p.table.reservedFor(5, clock.monotonicNs() + 2));
    const g5b = try p.verbs.acquireSlots(gpa, .{ .appender_id = 2, .explicit = &.{5} });
    gpa.free(g5b.granted.grants);
    try testing.expect(p.table.view(5).leasedBy(2));
}
