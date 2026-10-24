//! The HOLDER side of GPFS-strict read tokens (charter §5.5 "Tokens —
//! GPFS-strict, the ONLY foreign-read method"; `docs/porting/
//! sym_tokens_intents_striping.md` §1.1, §1.3 G/R/A/L, §1.4, §1.5, §1.12):
//! the per-object grant table (object = an inode with its carried xattrs,
//! or a directory's dentry set paged by cookie — never the slot), the
//! `GrantPassGate` register-then-check park, the recall-and-wait the
//! conveyor pass runs at A1½ with the lease verdicts and the departure
//! sweep, the grant page builder under the ONE byte budget, and the
//! holder-side `dlm_token_*` family. PORTABLE: the device is behind
//! `RecordSource`, the wire behind `Pusher`, the membership owner behind
//! `LeaseOracle`.
//!
//! The two tables are two mutexed maps (the lane and the gate); each side
//! takes BOTH locks one per step, so the locks' acquire/release chains
//! order the steps and no extra fence is claimed — `grant_gate_model.zig`
//! proves the law and both weakenings. A grant concurrent with a pass is
//! either RECALLED (the pass saw the holder) or PARKED to the settle and
//! served the post-commit records (the grant saw the mark) — never
//! neither.
//!
//! Measured (`.benchmarks/…pr5-tokens.md` §3, the D1 arithmetic): the
//! pass's recall of a 64-reader broadcast = 2.30 ms (`send` 1.38 +
//! `drain` 0.40 + `ack` 0.51 ms, in-process loopback, debug) — the cost a
//! `setattr` of an object 64 members cache pays before its ring admission;
//! `free_grace_hold_ms` under tokens ≈ 2.3 ms against the 2 724 ms
//! epoch-fan-in composite it replaced.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const log = @import("../../../core/log.zig");
const clock = @import("../../../core/clock.zig");
const knobs = @import("../../../core/knobs.zig");
const sync = @import("../../../sync/sync.zig");
const verbs = @import("../../../wire/verbs.zig");
const codec = @import("../../../wire/codec.zig");
const frame = @import("../../../wire/frame.zig");
const record = @import("../../record.zig");
const key = @import("../../key.zig");
const tx = @import("../../tx.zig");

const assert = std.debug.assert;
const ns_per_ms: u64 = std.time.ns_per_ms;
const ns_per_s: u64 = std.time.ns_per_s;

pub const Object = u64;
pub const ClientId = u64;

/// `Holder.prepare`: what runs before a grant of `object` is served.
pub const PrepareSeam = struct {
    ctx: *anyopaque,
    run: *const fn (ctx: *anyopaque, io: Io, object: Object) void,
};

// ---------------------------------------------------------------------------
// Constants — value and WHY.
// ---------------------------------------------------------------------------

/// `T_owner`, the membership lease TTL (charter §5.5 "home shard evicts
/// at `T_owner` (45 s)"): the recall DEADLINE on the token lane by
/// derivation — never a p99 ("a process-wide evidence read made one
/// suite's fast acks the next suite's 1 ms deadline"); past the TTL the
/// fence arithmetic bounds the client anyway (decision 0002 row 258).
pub const T_OWNER_NS: u64 = 45 * ns_per_s;
pub const RECALL_DEADLINE_NS: u64 = T_OWNER_NS;
/// Half the control frame for the xattrs + dentries of ONE grant page,
/// half for attrs/framing/MAC — the encode check's conservative split.
/// "One value (≤ the record cap) always fits an empty page."
pub const GRANT_PAGE_BUDGET: usize = frame.CONTROL_CAP / 2;
/// The reader charges 128 B per cached entry (entry + bucket + index
/// header) and 24 B per dentry (cookie + ino + type + index slot); the
/// holder charges the dentry page the same so a page never exceeds what
/// the reader will account for it.
pub const TOKEN_ENTRY_BYTES: u64 = 128;
pub const DIR_RECORD_BYTES: u64 = 24;
/// The expiry tick floor: below 1 ms a re-read of an UNKNOWN-remaining
/// lease degenerates into a spin; acks and the earliest live expiry wake
/// the pass earlier.
pub const EXPIRY_TICK_FLOOR_NS: u64 = 1 * ns_per_ms;

/// The pass's park tick: `max(deadline / 4, 1 ms)` — bounds how late an
/// unknown-remaining lease is re-read.
pub fn expiryTickNs(deadline_ns: u64) u64 {
    return @max(deadline_ns / 4, EXPIRY_TICK_FLOOR_NS);
}

comptime {
    assert(GRANT_PAGE_BUDGET == 512 * 1024);
    assert(verbs.RECALL_BATCH_MAX == 16_384);
    assert(expiryTickNs(RECALL_DEADLINE_NS) == 11_250 * ns_per_ms);
}

// ---------------------------------------------------------------------------
// Instruments — the holder-side `dlm_token_*` family.
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var dlm_token_grants_served: counters.ShardedCounter = .{};
    /// Grants recalled; closure `recalls ≡ recall_acks + recall_expired_
    /// with_lease + recall_timeouts_live` per holder at rest.
    pub var dlm_token_recalls: counters.ShardedCounter = .{};
    pub var dlm_token_recall_acks: counters.ShardedCounter = .{};
    /// A recall completed at the reader's lease expiry (or an UNKNOWN
    /// lease past the deadline): the token is retired, the commit proceeds.
    pub var dlm_token_recall_expired_with_lease: counters.ShardedCounter = .{};
    /// A recall unacked by a LIVE member past its lease TTL (must-stay-0):
    /// the token is retired and the commit PROCEEDS — the fence
    /// arithmetic bounds the client.
    pub var dlm_token_recall_timeouts_live: counters.Tripwire = .init("dlm_token_recall_timeouts_live");
    /// Grants a departed/evicted member lost at the owner's word (not
    /// under a recall — those complete as `expired_with_lease`).
    pub var dlm_token_lease_swept_grants: counters.ShardedCounter = .{};
    /// A grant for a slot another appender leases: `NotHolder{holder}`.
    pub var dlm_token_not_holder_redirects: counters.ShardedCounter = .{};
    /// A grant for a slot whose lessee the owner lists DEAD: `HolderDead`,
    /// never a dial.
    pub var dlm_token_holder_dead_redirects: counters.ShardedCounter = .{};
    /// A member whose lease the owner says EXPIRED reached a token verb:
    /// `NotMember`, nothing registered. A state the protocol answers, not
    /// a fault (decision 0003 §AE.3): a frame the member sent while live is
    /// served after its deadline whenever its session queued it — behind
    /// its own grant parked on its own pending recall, which completes only
    /// at the lease's expiry (the shared hot directory row's killed holder).
    pub var dlm_token_nonmember_refusals: counters.ShardedCounter = .{};
    /// A `CustodyGrant` whose words the screen refused (must-stay-0).
    pub var dlm_token_custody_rejected: counters.Tripwire = .init("dlm_token_custody_rejected");
    pub var dlm_token_releases: counters.ShardedCounter = .{};
    /// Passes that recalled anything (ONE batch per pass whatever the
    /// union's size — 16 co-queued commits on one object are one batch).
    pub var dlm_token_recall_batches: counters.ShardedCounter = .{};
    /// Grants parked on an object a pass had in flight.
    pub var dlm_token_grant_parks: counters.ShardedCounter = .{};
    /// Grants that waited out a pending recall of their own `(client,
    /// object)` before registering afresh.
    pub var dlm_token_regrant_under_recall_waits: counters.ShardedCounter = .{};
    /// Grants a pass found past the recall deadline while parked: refused
    /// EAGAIN, never served pre-commit records.
    pub var dlm_token_grant_park_refusals: counters.ShardedCounter = .{};
    /// Live grants across every object (the table's population).
    pub var dlm_token_outstanding: counters.Gauge = .{};
    /// Grants a recall's ack KEPT registered for the kernel pin (decision
    /// 0003 §AG.2 at the holder): the reader's kernel may still serve
    /// what it was told under the token, so the next change recalls it.
    pub var dlm_token_recall_pins: counters.ShardedCounter = .{};
    /// Pinned grants that left the table at their pin's end unclaimed.
    pub var dlm_token_recall_pins_expired: counters.ShardedCounter = .{};
    /// Readers recalled per object per batch: buckets 1–8 exact, ≤16, ≤32,
    /// ≤64, >64 (the conveyor's group-size law; p99 = R in the broadcast
    /// shape).
    pub var dlm_token_recall_fanout: [12]counters.ShardedCounter = @splat(.{});
    /// `dlm_token_recall_rtt_ns` EXACT-SUM per batch: `send` = call → the
    /// last frame pushed, `drain` = → the last ack's arrival, `ack` = →
    /// the pass observing it; cuts clamped monotone into `[t0, t_end]` so
    /// `send + drain + ack ≡ total` to the nanosecond. Measured 64-reader
    /// broadcast: 1 384 804 + 401 135 + 514 468 = 2 300 407 ns.
    pub var dlm_token_recall_rtt_send: counters.ShardedLatencyHistogram = .{};
    pub var dlm_token_recall_rtt_drain: counters.ShardedLatencyHistogram = .{};
    pub var dlm_token_recall_rtt_ack: counters.ShardedLatencyHistogram = .{};
    pub var dlm_token_recall_rtt_total: counters.ShardedLatencyHistogram = .{};
};

pub fn fanoutBucket(n: usize) usize {
    if (n <= 8) return @max(n, 1) - 1;
    if (n <= 16) return 8;
    if (n <= 32) return 9;
    if (n <= 64) return 10;
    return 11;
}

// ---------------------------------------------------------------------------
// Seams.
// ---------------------------------------------------------------------------

/// The membership owner's word on a client (spec §1.12 `LeaseVerdict`):
/// `live` (deadline ahead of the owner's clock), `expired` (past it — or
/// not listed: any holder the owner does not list has LEFT or been
/// EVICTED), `unknown` (no owner installed — the in-process contracts:
/// waited for like live, expiry counted with the lease).
pub const LeaseVerdict = enum { live, expired, unknown };

pub const LeaseOracle = struct {
    ctx: ?*anyopaque,
    /// `since_ns`: the instant the client was last in contact with this
    /// holder (its latest grant ask; `now_ns` for an ask in hand) — a
    /// word OLDER than that contact cannot call the client's absence a
    /// death (a joiner's census view; the owner's roster ignores it).
    verdict: *const fn (ctx: ?*anyopaque, client: ClientId, since_ns: u64, now_ns: u64) LeaseVerdict,
    /// Remaining ns of a live lease at `now`; null = unknown.
    remaining: *const fn (ctx: ?*anyopaque, client: ClientId, now_ns: u64) ?u64,

    /// No owner installed: every client unknown.
    pub const none: LeaseOracle = .{ .ctx = null, .verdict = &unknownVerdict, .remaining = &noRemaining };

    fn unknownVerdict(_: ?*anyopaque, _: ClientId, _: u64, _: u64) LeaseVerdict {
        return .unknown;
    }
    fn noRemaining(_: ?*anyopaque, _: ClientId, _: u64) ?u64 {
        return null;
    }
};

/// What a pushed frame came back as (the reader's reply, or its absence).
pub const PushVerdict = enum {
    /// The push's reply carried the ack.
    acked,
    /// No session to the reader (unbound endpoint, a dead dial): the frame
    /// stays pending until the reader's lease expiry retires it.
    unreachable_,
};

/// The names a recall carries (decision 0003 §AB.2): the dentries the
/// batch changes, each under its parent object; `unnamed` = the batch
/// changed a dentry no note names (the reader drops every dentry).
pub const RecallNames = struct {
    notes: []const tx.NameNote = &.{},
    unnamed: bool = false,
};

/// The wire: ONE push of one frame to one reader (the holder dials the
/// reader's listener). `names` are the frame's: those under its objects.
pub const Pusher = struct {
    ctx: *anyopaque,
    push: *const fn (ctx: *anyopaque, client: ClientId, frame_id: u64, objects: []const Object, names: RecallNames) PushVerdict,
};

/// The notes whose parent is one of `objects` (sorted ascending): a
/// reader is told only the names under the objects it held.
fn namesUnder(gpa: Allocator, objects: []const Object, names: RecallNames) Allocator.Error!RecallNames {
    var out: std.ArrayListUnmanaged(tx.NameNote) = .empty;
    errdefer out.deinit(gpa);
    for (names.notes) |n| {
        if (std.sort.binarySearch(Object, objects, n.parent, orderObject) == null) continue;
        try out.append(gpa, n);
    }
    return .{ .notes = try out.toOwnedSlice(gpa), .unnamed = names.unnamed };
}

fn orderObject(a: Object, b: Object) std.math.Order {
    return std.math.order(a, b);
}

pub const SourceError = error{ Io, Corrupt, OutOfMemory };

pub const XattrRec = struct { name: []const u8, value: []const u8 };
pub const DirRec = struct { cookie: u64, child_ino: u64, file_type: u8, name: []const u8 };

/// The holder's records (the backend's trees; a RAM store in the
/// contracts): the FOLDED inode (pending times folded in), the carried
/// xattr set SORTED by name, the dentries in cookie order.
pub const RecordSource = struct {
    ctx: *anyopaque,
    /// null = absent (`Gone`).
    inode: *const fn (ctx: *anyopaque, object: Object) SourceError!?record.InodeValue,
    /// Every carried xattr of `object` in name order, appended to `out`
    /// (bytes owned by `arena`).
    xattrs: *const fn (ctx: *anyopaque, arena: Allocator, object: Object, out: *std.ArrayList(XattrRec)) SourceError!void,
    /// Up to `max` dentries of directory `object` in cookie order STRICTLY
    /// after cookie `after` (0 = start); true when more remain past them.
    dentries: *const fn (ctx: *anyopaque, arena: Allocator, object: Object, after: u64, max: usize, out: *std.ArrayList(DirRec)) SourceError!bool,
};

/// `token_carried_xattr(name)`: the user-visible allowlist ∪ `layout` ∪
/// `system.symlink`. Every other internal record (`writer_claim`,
/// `client:`, `job:`, the planes' own) is the CONTROL plane's — projected,
/// never tokened, and its write recalls nothing (`KvTx.token_quiet`).
pub fn isCarriedXattr(name: []const u8) bool {
    if (std.mem.eql(u8, name, "layout") or std.mem.eql(u8, name, "system.symlink")) return true;
    if (std.mem.startsWith(u8, name, "user.")) return !std.mem.startsWith(u8, name, "user.squeezefs.");
    return std.mem.startsWith(u8, name, "security.") or std.mem.startsWith(u8, name, "trusted.");
}

/// The object a slot-tree record names (`record_object_ino`): the ino at
/// the kind's routing offset — inodes, dentries BY PARENT, xattrs, block
/// map; refs by their OWNER ino. v4 has ONE key layout, so one codec
/// answers every kind (decision 0002 row 260). Null for a non-slot-tree
/// kind (tree 0's control records recall nothing).
pub fn recordObjectIno(tree_id: u8, k: []const u8) ?Object {
    if (!key.isSlotTreeKind(tree_id)) return null;
    const off = key.routeOff(tree_id);
    if (k.len < off + 8) return null;
    return std.mem.readInt(u64, k[off..][0..8], .big);
}

/// The UNION of a batch's token objects: every record of every member
/// that is not `token_quiet`, sorted and deduped at `finish` — but an
/// ACCESS STAMP (an inode Δ naming the atime alone: a read's touch, shipped
/// or this mount's own) recalls no reader: it changes no name, no byte and
/// no other attribute, and the kernel re-asks a read's atime at its attr
/// TTL anyway (`fuse_invalidate_atime`). A reader's token keeps the
/// pre-stamp atime until the object's next real change (lazy, as every
/// cluster filesystem's atime); the toucher keeps its own stamp
/// (`token_cache.Entry.raiseAtime`).
pub const ObjectUnion = struct {
    list: std.ArrayList(Object) = .empty,

    pub fn addMember(self: *ObjectUnion, gpa: Allocator, q: *const tx.QueuedTx) Allocator.Error!void {
        if (q.token_quiet) return;
        for (q.recs) |r| {
            if (isAccessStamp(r)) continue;
            if (recordObjectIno(r.tree_id, r.key)) |o| try self.list.append(gpa, o);
        }
    }

    fn isAccessStamp(r: tx.Rec) bool {
        return r.tree_id == key.TREE_INODES and r.kind == .delta and record.inodeDeltaMask(r.value) == record.DELTA_ATIME;
    }

    /// Sort + dedupe in place; the slice is the union.
    pub fn finish(self: *ObjectUnion) []const Object {
        std.mem.sort(Object, self.list.items, {}, std.sort.asc(Object));
        var w: usize = 0;
        for (self.list.items) |o| {
            if (w != 0 and self.list.items[w - 1] == o) continue;
            self.list.items[w] = o;
            w += 1;
        }
        self.list.items.len = w;
        return self.list.items;
    }

    pub fn deinit(self: *ObjectUnion, gpa: Allocator) void {
        self.list.deinit(gpa);
    }
};

// ---------------------------------------------------------------------------
// Test seams (registry rows read ONCE at the arm, never process statics —
// decision 0002 row 259).
// ---------------------------------------------------------------------------

pub const Seams = struct {
    /// The recall deadline instead of `T_owner` (0 = the constant): the
    /// live-timeout contract completes inside a test's budget.
    recall_deadline_ns: u64 = 0,
    /// The pass holds after its recall completed, before it proceeds, until
    /// `release_hold` — the window a first-touch grant must PARK in.
    conveyor_hold_post_recall: bool = false,
    /// A grant holds between its gate check and its read until the hold is
    /// released — the window a pass takes the object in flight DURING the
    /// read (the re-park + re-read arm).
    grant_hold_after_check: bool = false,
    /// `SQUEEZEFS_TEST_TOKEN_RECALL_HOLD`: the FIFO the first pass whose
    /// recall pushed a frame parks on after its acks (the daemon's arm of
    /// the hold above: the requester's op in flight, its recall acked).
    recall_hold_fifo: ?[]const u8 = null,

    pub fn fromEnv(env: *const std.process.Environ.Map) Seams {
        return .{
            .recall_deadline_ns = knobs.intKnob("SQUEEZEFS_TEST_TOKEN_RECALL_DEADLINE_MS", u64, env) * ns_per_ms,
            .conveyor_hold_post_recall = knobs.boolKnob("SQUEEZEFS_TEST_TOKEN_CONVEYOR_HOLD_POST_RECALL", env),
            .grant_hold_after_check = knobs.boolKnob("SQUEEZEFS_TEST_TOKEN_GRANT_HOLD_AFTER_CHECK", env),
            .recall_hold_fifo = knobs.parseStr(env.get("SQUEEZEFS_TEST_TOKEN_RECALL_HOLD")),
        };
    }
};

// ---------------------------------------------------------------------------
// The GrantPassGate — register-then-check (spec §1.5).
// ---------------------------------------------------------------------------

pub const Admission = enum { proceed, park };

pub const GrantPassGate = struct {
    gpa: Allocator,
    lock: sync.Mutex = .init,
    /// A REFCOUNT per object, never a set: two overlapping users (a pass
    /// and a failed window's rollback) keep the object in flight until the
    /// LAST settles.
    inflight: std.AutoHashMapUnmanaged(Object, u32) = .empty,
    /// Parked grants wake here at every settle.
    settled: sync.Notify = .init,

    pub fn init(gpa: Allocator) GrantPassGate {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *GrantPassGate) void {
        self.inflight.deinit(self.gpa);
    }

    /// Mark every object of a pass's union in flight — under ONE lock,
    /// BEFORE the pass reads the holders through the lane's lock.
    pub fn passBegin(self: *GrantPassGate, io: Io, objects: []const Object) Allocator.Error!void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        try self.inflight.ensureUnusedCapacity(self.gpa, @intCast(objects.len));
        for (objects) |o| {
            const gop = self.inflight.getOrPutAssumeCapacity(o);
            if (!gop.found_existing) gop.value_ptr.* = 0;
            gop.value_ptr.* += 1;
        }
    }

    /// The grant's check AFTER its registration in the lane.
    pub fn admission(self: *GrantPassGate, io: Io, object: Object) Admission {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        return if (self.inflight.contains(object)) .park else .proceed;
    }

    /// The window's terminal outcome (fan-out AND rollback): the count
    /// drops; an object leaves the flight with its LAST user; parked
    /// grants wake.
    pub fn settle(self: *GrantPassGate, io: Io, objects: []const Object) void {
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            for (objects) |o| {
                const p = self.inflight.getPtr(o) orelse continue;
                p.* -= 1;
                if (p.* == 0) _ = self.inflight.remove(o);
            }
        }
        self.settled.notifyWaiters(io);
    }

    pub fn inflightCount(self: *GrantPassGate, io: Io) usize {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        return self.inflight.count();
    }
};

// ---------------------------------------------------------------------------
// The holder: the lane (grants, frames, clients) + the gate.
// ---------------------------------------------------------------------------

pub const Error = error{
    /// The grant table is at its cap (a reader retries; R5's shed on the
    /// reader side returns room by voluntary releases).
    TableFull,
    /// The park outlived the recall deadline: refused, never pre-commit
    /// records.
    ParkExpired,
    OutOfMemory,
    Io,
    Corrupt,
};

pub fn coreError(e: Error) errors.Error {
    return switch (e) {
        error.TableFull, error.ParkExpired => error.Deferred,
        error.OutOfMemory => error.OutOfMemory,
        error.Io, error.Corrupt => error.InputOutput,
    };
}

const Grant = struct {
    client: ClientId,
    under_recall: bool = false,
    /// A recalled grant stays registered until this instant (decision
    /// 0003 §AG.2's pin, at the holder): the reader's kernel may serve
    /// what it was told under the token for the kernel's longest TTL, and
    /// only a registered reader hears the next change's names. 0 = live.
    recalled_until_ns: u64 = 0,
};

/// A pinned-recalled grant's expiry, FIFO (pins are one length, so acks
/// enqueue in time order); a grant re-registered or re-recalled since
/// leaves a stale entry the sweep skips (its `until` no longer matches).
const Recalled = struct { object: Object, client: ClientId, until_ns: u64 };

const Holders = struct {
    list: std.ArrayListUnmanaged(Grant) = .empty,

    fn find(self: *const Holders, client: ClientId) ?usize {
        for (self.list.items, 0..) |g, i| if (g.client == client) return i;
        return null;
    }

    fn underRecall(self: *const Holders) u32 {
        var n: u32 = 0;
        for (self.list.items) |g| n += @intFromBool(g.under_recall);
        return n;
    }
};

const Frame = struct {
    frame_id: u64,
    objects: std.ArrayListUnmanaged(Object) = .empty,
    issued_ns: u64,
    deadline_ns: u64,
};

const Client = struct {
    grants: u32 = 0,
    /// The client's latest grant ask (the lease oracle's `since_ns`).
    contact_ns: u64 = 0,
    /// At most ONE in-flight frame per client: the pass is serialized per
    /// volume and waits for the frames it issued before it proceeds.
    frame: ?Frame = null,
};

pub const GrantAsk = struct {
    client: ClientId,
    object: Object,
    wants_dentries: bool,
    wants_records: bool,
    /// The last cookie carried (0 = start).
    after: u64,
    /// The last xattr name carried ("" = start).
    xattr_after: []const u8,
};

/// A built page: the head and the two tails inside the caller's scratch.
pub const Page = struct {
    head: verbs.GrantPageHead,
    xattrs_bytes: []const u8,
    dir_bytes: []const u8,

    pub fn wire(self: Page) verbs.GrantPage {
        return .{ .head = self.head, .xattrs_bytes = self.xattrs_bytes, .dir_bytes = self.dir_bytes };
    }
};

pub const GrantReply = union(enum) {
    granted: Page,
    /// The object has no record at its holder (a fresh registration is
    /// retracted).
    gone,
    /// The caller's membership lease is expired at the owner: nothing
    /// registered.
    not_member,
};

pub const RecallSummary = struct {
    /// Grants recalled in this batch.
    recalled: u32 = 0,
    acked: u32 = 0,
    expired_with_lease: u32 = 0,
    timeouts_live: u32 = 0,
    /// Distinct readers pushed.
    frames: u32 = 0,
};

pub const Holder = struct {
    gpa: Allocator,
    io: Io,
    /// This holder's identity word (the `holder` field of every frame).
    identity: ClientId,
    oracle: LeaseOracle,
    seams: Seams = .{},
    gate: GrantPassGate,
    lock: sync.Mutex = .init,
    objects: std.AutoHashMapUnmanaged(Object, Holders) = .empty,
    clients: std.AutoHashMapUnmanaged(ClientId, Client) = .empty,
    /// The table's caps: objects and clients, sized once from the budget
    /// (`cap_objects`) and the membership census (`cap_clients`).
    cap_objects: u32,
    cap_clients: u32,
    next_frame_id: u64 = 1,
    /// Acks, retirements and sweeps wake the pass AND the grants waiting
    /// out a pending recall.
    ack_wake: sync.Notify = .init,
    /// The RTT cut points of the batch in flight.
    last_send_ns: u64 = 0,
    last_ack_ns: u64 = 0,
    /// The test holds (`Seams`): released by `releaseHold`.
    hold: std.atomic.Value(u32) = .init(0),
    hold_wake: sync.Notify = .init,
    /// The FIFO hold fired (it fires ONCE).
    recall_hold_done: std.atomic.Value(bool) = .init(false),
    /// Published while a hold is standing (the test's deterministic window).
    holding: std.atomic.Value(u32) = .init(0),
    /// The write cache's RECALL (charter §6.1 "coherence", 2026-10-21):
    /// run before a foreign reader's grant of an object is served — the
    /// holder promotes its staged residents of that ino to the data volumes
    /// so the page names a durable copy; a foreign reader never reads zeros
    /// for staged bytes that exist (`plane.recallResidents`).
    prepare: ?PrepareSeam = null,
    /// How long a recalled grant stays registered (`setKernelPin`: the
    /// mount's longest peered kernel TTL, decision 0003 §AG.2); 0 = a
    /// recall's ack unregisters at once (a holder whose readers cache
    /// nothing — the portable contracts' default).
    kernel_pin_ns: u64 = 0,
    recalled: std.ArrayListUnmanaged(Recalled) = .empty,

    pub fn init(gpa: Allocator, io: Io, identity: ClientId, oracle: LeaseOracle, cap_objects: u32, cap_clients: u32) Allocator.Error!Holder {
        var self: Holder = .{ .gpa = gpa, .io = io, .identity = identity, .oracle = oracle, .gate = GrantPassGate.init(gpa), .cap_objects = cap_objects, .cap_clients = cap_clients };
        errdefer self.deinit();
        try self.objects.ensureTotalCapacity(gpa, cap_objects);
        try self.clients.ensureTotalCapacity(gpa, cap_clients);
        return self;
    }

    pub fn deinit(self: *Holder) void {
        var it = self.objects.valueIterator();
        while (it.next()) |h| h.list.deinit(self.gpa);
        self.objects.deinit(self.gpa);
        var ct = self.clients.valueIterator();
        while (ct.next()) |c| if (c.frame) |*f| f.objects.deinit(self.gpa);
        self.clients.deinit(self.gpa);
        self.recalled.deinit(self.gpa);
        self.gate.deinit();
    }

    /// The kernel pin (decision 0003 §AG.2): a reader whose kernel was
    /// answered under a token keeps hearing this holder's changes for
    /// `ns` past a recall, so a change to a name the reader's kernel still
    /// holds reaches it by name — the token's coherence lasts as long as
    /// its answers may. The set's mounts share one TTL posture, so the
    /// holder's own longest peered class is every reader's.
    pub fn setKernelPin(self: *Holder, io: Io, ns: u64) void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        self.kernel_pin_ns = ns;
    }

    pub fn recallDeadlineNs(self: *const Holder) u64 {
        return if (self.seams.recall_deadline_ns != 0) self.seams.recall_deadline_ns else RECALL_DEADLINE_NS;
    }

    // ---- the lane (every function below runs under `lock`) ----

    /// Register `(object, client)` asked at `now_ns`; true when it was
    /// already held.
    fn registerLocked(self: *Holder, object: Object, client: ClientId, now_ns: u64) Error!bool {
        const c = self.clients.getPtr(client) orelse blk: {
            if (self.clients.count() >= self.cap_clients) return error.TableFull;
            self.clients.putAssumeCapacity(client, .{});
            break :blk self.clients.getPtr(client).?;
        };
        c.contact_ns = @max(c.contact_ns, now_ns);
        const h = self.objects.getPtr(object) orelse blk: {
            if (self.objects.count() >= self.cap_objects) return error.TableFull;
            self.objects.putAssumeCapacity(object, .{});
            break :blk self.objects.getPtr(object).?;
        };
        if (h.find(client)) |i| {
            // A fresh ask on a pinned-recalled grant: live again.
            h.list.items[i].recalled_until_ns = 0;
            return true;
        }
        try h.list.append(self.gpa, .{ .client = client });
        c.grants += 1;
        stats.dlm_token_outstanding.add(1);
        return false;
    }

    /// Drop `(object, client)`; true when it was held.
    fn unregisterLocked(self: *Holder, object: Object, client: ClientId) bool {
        const h = self.objects.getPtr(object) orelse return false;
        const i = h.find(client) orelse return false;
        _ = h.list.swapRemove(i);
        if (h.list.items.len == 0) {
            h.list.deinit(self.gpa);
            _ = self.objects.remove(object);
        }
        if (self.clients.getPtr(client)) |c| c.grants -= 1;
        stats.dlm_token_outstanding.sub(1);
        return true;
    }

    fn pendingRecallLocked(self: *Holder, object: Object, client: ClientId) bool {
        const c = self.clients.getPtr(client) orelse return false;
        const f = &(c.frame orelse return false);
        for (f.objects.items) |o| if (o == object) return true;
        return false;
    }

    /// Retire a client's frame: every object it names leaves the table.
    /// The frame's grants retire: unregistered, or — an ACK at a holder
    /// with a kernel pin (`pin_from_ns` = the ack's instant) — kept
    /// registered until the pin's end, so the next change recalls the
    /// reader again while its kernel may still serve the token's answers.
    /// An expiry or a sweep (`pin_from_ns` null) unregisters: a reader
    /// that stopped acking is owed nothing.
    fn retireFrameLocked(self: *Holder, client: ClientId, pin_from_ns: ?u64) u32 {
        const c = self.clients.getPtr(client) orelse return 0;
        var f = c.frame orelse return 0;
        c.frame = null;
        var n: u32 = 0;
        for (f.objects.items) |o| {
            if (pin_from_ns != null and self.kernel_pin_ns != 0) {
                if (self.pinRecalledLocked(o, client, pin_from_ns.? +| self.kernel_pin_ns)) {
                    n += 1;
                    continue;
                }
            }
            n += @intFromBool(self.unregisterLocked(o, client));
        }
        f.objects.deinit(self.gpa);
        return n;
    }

    /// Keep `(object, client)` registered until `until_ns`; false when it
    /// is not held (or the expiry list has no room: the grant leaves now).
    /// A grant the recall found ALREADY pinned keeps its pin's end: the
    /// reader was told nothing under the token since its first recall (a
    /// new answer takes a fresh grant, which makes the grant live), so its
    /// kernel's answers end by then — re-pinning from every ack kept a
    /// reader that holds nothing on every change of a directory changed
    /// faster than the pin, one recall per create.
    fn pinRecalledLocked(self: *Holder, object: Object, client: ClientId, until_ns: u64) bool {
        const h = self.objects.getPtr(object) orelse return false;
        const i = h.find(client) orelse return false;
        const g = &h.list.items[i];
        if (g.recalled_until_ns == 0) {
            self.recalled.append(self.gpa, .{ .object = object, .client = client, .until_ns = until_ns }) catch return false;
            g.recalled_until_ns = until_ns;
        }
        g.under_recall = false;
        stats.dlm_token_recall_pins.inc();
        return true;
    }

    /// An object about to be recalled: its pinned grants past their pin
    /// leave now rather than hear one more recall (exact whatever the
    /// FIFO's order; the sweep below is the bulk reclaimer).
    fn dropExpiredPinsLocked(self: *Holder, object: Object, now_ns: u64) void {
        var i: usize = 0;
        while (true) {
            const h = self.objects.getPtr(object) orelse return;
            if (i >= h.list.items.len) return;
            const g = h.list.items[i];
            if (g.recalled_until_ns != 0 and g.recalled_until_ns <= now_ns) {
                _ = self.unregisterLocked(object, g.client);
                stats.dlm_token_recall_pins_expired.inc();
                continue;
            }
            i += 1;
        }
    }

    /// Pinned-recalled grants past their pin leave the table: the FIFO's
    /// front while expired (one pin length ⇒ time order); a stale entry
    /// (re-registered, re-recalled or dropped since) is skipped.
    fn sweepPinsLocked(self: *Holder, now_ns: u64) void {
        var drop: usize = 0;
        while (drop < self.recalled.items.len and self.recalled.items[drop].until_ns <= now_ns) : (drop += 1) {
            const r = self.recalled.items[drop];
            const h = self.objects.getPtr(r.object) orelse continue;
            const i = h.find(r.client) orelse continue;
            if (h.list.items[i].recalled_until_ns != r.until_ns) continue;
            if (self.unregisterLocked(r.object, r.client)) stats.dlm_token_recall_pins_expired.inc();
        }
        if (drop != 0) self.recalled.replaceRange(self.gpa, 0, drop, &.{}) catch unreachable;
    }

    /// A client the owner says EXPIRED loses every grant NOW; its pending
    /// frame completes as `expired_with_lease`.
    fn sweepClientLocked(self: *Holder, client: ClientId, summary: ?*RecallSummary) void {
        const c = self.clients.getPtr(client) orelse return;
        if (c.frame != null) {
            const n = self.retireFrameLocked(client, null);
            stats.dlm_token_recall_expired_with_lease.add(n);
            if (summary) |s| s.expired_with_lease += n;
        }
        var swept: u32 = 0;
        var it = self.objects.iterator();
        var gone: std.ArrayListUnmanaged(Object) = .empty;
        defer gone.deinit(self.gpa);
        while (it.next()) |e| {
            if (e.value_ptr.find(client)) |_| gone.append(self.gpa, e.key_ptr.*) catch break;
        }
        for (gone.items) |o| swept += @intFromBool(self.unregisterLocked(o, client));
        stats.dlm_token_lease_swept_grants.add(swept);
        _ = self.clients.remove(client);
    }

    fn sweepExpiredLocked(self: *Holder, now_ns: u64, summary: ?*RecallSummary) void {
        self.sweepPinsLocked(now_ns);
        var expired: std.ArrayListUnmanaged(ClientId) = .empty;
        defer expired.deinit(self.gpa);
        var it = self.clients.iterator();
        while (it.next()) |e| {
            if (self.oracle.verdict(self.oracle.ctx, e.key_ptr.*, e.value_ptr.contact_ns, now_ns) == .expired) expired.append(self.gpa, e.key_ptr.*) catch break;
        }
        for (expired.items) |c| self.sweepClientLocked(c, summary);
    }

    /// Frames past the deadline: a client judged `expired | unknown` is
    /// `expired_with_lease`; a LIVE client is the must-stay-0 — the token
    /// is retired and the commit PROCEEDS either way.
    fn expireOverdueLocked(self: *Holder, now_ns: u64, summary: *RecallSummary) void {
        var overdue: std.ArrayListUnmanaged(ClientId) = .empty;
        defer overdue.deinit(self.gpa);
        var it = self.clients.iterator();
        while (it.next()) |e| {
            if (e.value_ptr.frame) |f| if (now_ns >= f.deadline_ns) overdue.append(self.gpa, e.key_ptr.*) catch break;
        }
        for (overdue.items) |c| {
            const verdict = self.oracle.verdict(self.oracle.ctx, c, self.clients.get(c).?.contact_ns, now_ns);
            const n = self.retireFrameLocked(c, null);
            if (verdict == .live) {
                stats.dlm_token_recall_timeouts_live.fire("a recall stood unacked past the lease TTL by a member the owner lists LIVE; the token is retired and the commit proceeds");
                summary.timeouts_live += n;
            } else {
                stats.dlm_token_recall_expired_with_lease.add(n);
                summary.expired_with_lease += n;
            }
        }
    }

    fn outstandingLocked(self: *Holder, objects: []const Object) u32 {
        var n: u32 = 0;
        for (objects) |o| if (self.objects.getPtr(o)) |h| {
            n += h.underRecall();
        };
        return n;
    }

    /// The earliest wake the pass must take: the nearest frame deadline,
    /// the earliest LIVE lease's remaining, the tick.
    fn nextWakeLocked(self: *Holder, now_ns: u64) u64 {
        var wake = now_ns + expiryTickNs(self.recallDeadlineNs());
        var it = self.clients.iterator();
        while (it.next()) |e| {
            const f = e.value_ptr.frame orelse continue;
            wake = @min(wake, f.deadline_ns);
            if (self.oracle.remaining(self.oracle.ctx, e.key_ptr.*, now_ns)) |rem| wake = @min(wake, now_ns + rem);
        }
        return wake;
    }

    // ---- the protocol ----

    /// The pass's A1½: take the union in flight (mark THEN read), sweep
    /// dead readers' grants, recall every holder ONCE, push ONE frame per
    /// reader, wait on every ack or the reader's lease expiry. The caller
    /// settles the SAME union at the window's terminal outcome.
    pub fn recallAndWait(self: *Holder, io: Io, objects: []const Object, names: RecallNames, pusher: Pusher) Allocator.Error!RecallSummary {
        var summary: RecallSummary = .{};
        const t0 = clock.monotonicNs();
        try self.gate.passBegin(io, objects);
        const deadline = self.recallDeadlineNs();
        var clients_hit: std.ArrayListUnmanaged(ClientId) = .empty;
        defer clients_hit.deinit(self.gpa);
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            self.sweepExpiredLocked(t0, &summary);
            self.last_send_ns = t0;
            self.last_ack_ns = t0;
            for (objects) |o| {
                self.dropExpiredPinsLocked(o, t0);
                const h = self.objects.getPtr(o) orelse continue;
                var fanout: usize = 0;
                for (h.list.items) |*g| {
                    if (g.under_recall) continue;
                    const c = self.clients.getPtr(g.client).?;
                    if (c.frame == null) {
                        c.frame = .{ .frame_id = self.next_frame_id, .issued_ns = t0, .deadline_ns = t0 + deadline };
                        self.next_frame_id += 1;
                        try clients_hit.append(self.gpa, g.client);
                    }
                    try c.frame.?.objects.append(self.gpa, o);
                    g.under_recall = true;
                    fanout += 1;
                }
                if (fanout != 0) {
                    stats.dlm_token_recall_fanout[fanoutBucket(fanout)].inc();
                    summary.recalled += @intCast(fanout);
                }
            }
        }
        if (summary.recalled == 0) {
            self.holdIfArmed(io, self.seams.conveyor_hold_post_recall);
            return summary;
        }
        stats.dlm_token_recalls.add(summary.recalled);
        stats.dlm_token_recall_batches.inc();
        summary.frames = @intCast(clients_hit.items.len);
        // Push one frame per reader; a frame the push acked inline retires
        // at once (the reply IS the ack).
        for (clients_hit.items) |c| {
            var frame_id: u64 = 0;
            var objs: []const Object = &.{};
            {
                self.lock.lockUncancelable(io);
                defer self.lock.unlock(io);
                const cl = self.clients.getPtr(c) orelse continue;
                const f = cl.frame orelse continue;
                frame_id = f.frame_id;
                objs = self.gpa.dupe(Object, f.objects.items) catch continue;
            }
            defer self.gpa.free(objs);
            const own = namesUnder(self.gpa, objs, names) catch RecallNames{ .unnamed = names.unnamed or names.notes.len != 0 };
            defer self.gpa.free(own.notes);
            const verdict = pusher.push(pusher.ctx, c, frame_id, objs, own);
            self.last_send_ns = clock.monotonicNs();
            if (verdict == .acked) _ = self.serveAck(io, c, frame_id);
        }
        // Wait for the recalls this pass ISSUED: a grant registered after
        // the union went in flight is parked on the gate, never counted.
        // The owner's word is read AFTER the wake is armed (register-then-
        // check): a census that lands between the two is swept here, one
        // that lands after wakes the wait (`sweepExpired`).
        while (true) {
            var n = self.ack_wake.notified();
            const pre = n.enable(io);
            var wake_at: u64 = 0;
            var done = false;
            const retired0 = summary.expired_with_lease + summary.timeouts_live;
            {
                self.lock.lockUncancelable(io);
                defer self.lock.unlock(io);
                const now = clock.monotonicNs();
                self.sweepExpiredLocked(now, &summary);
                self.expireOverdueLocked(now, &summary);
                done = self.outstandingLocked(objects) == 0;
                if (!done) wake_at = self.nextWakeLocked(now);
            }
            // A frame this pass retired at a lease's expiry or past the
            // deadline frees every grant parked on that recall (a serve
            // waits on the same wake an ack posts) — never left to its own
            // park deadline.
            if (summary.expired_with_lease + summary.timeouts_live != retired0) self.ack_wake.notifyWaiters(io);
            if (done) {
                n.cancel(io);
                break;
            }
            if (!pre) _ = n.waitUntilUncancelable(io, wake_at) else n.cancel(io);
        }
        const t_end = clock.monotonicNs();
        // The exact-sum cuts: clamp monotone into [t0, t_end].
        const t_send = std.math.clamp(self.last_send_ns, t0, t_end);
        const t_drain = std.math.clamp(self.last_ack_ns, t_send, t_end);
        stats.dlm_token_recall_rtt_send.record(t_send - t0);
        stats.dlm_token_recall_rtt_drain.record(t_drain - t_send);
        stats.dlm_token_recall_rtt_ack.record(t_end - t_drain);
        stats.dlm_token_recall_rtt_total.record(t_end - t0);
        summary.acked = @intCast(@as(u64, summary.recalled) -| summary.expired_with_lease -| summary.timeouts_live);
        self.holdIfArmed(io, self.seams.conveyor_hold_post_recall);
        if (self.seams.recall_hold_fifo) |path| if (!self.recall_hold_done.swap(true, .seq_cst)) holdOnFifo(io, path, summary);
        return summary;
    }

    /// The FIFO arm of the post-recall hold (`SQUEEZEFS_TEST_TOKEN_RECALL_HOLD`):
    /// park until a writer opens `path` (the row's release).
    fn holdOnFifo(io: Io, path: []const u8, summary: RecallSummary) void {
        log.info("tokens: the pass recalled {d} grant(s) in {d} frame(s), {d} acked: SQUEEZEFS_TEST_TOKEN_RECALL_HOLD — holding until a writer opens {s}", .{ summary.recalled, summary.frames, summary.acked, path });
        const f = Io.Dir.cwd().openFile(io, path, .{}) catch |e| {
            log.warn("tokens: the SQUEEZEFS_TEST_TOKEN_RECALL_HOLD hold could not open {s} ({t}); not holding", .{ path, e });
            return;
        };
        f.close(io);
    }

    /// The window's terminal outcome: the gate's refcount drops.
    pub fn settle(self: *Holder, io: Io, objects: []const Object) void {
        self.gate.settle(io, objects);
    }

    /// The ack of `client`'s frame `frame_id` (its push's reply): the
    /// frame's grants retire; a stale ack matches nothing (debug, never a
    /// refusal). Answers the grants retired.
    pub fn serveAck(self: *Holder, io: Io, client: ClientId, frame_id: u64) u32 {
        var n: u32 = 0;
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            const c = self.clients.getPtr(client);
            const live = if (c) |cl| (if (cl.frame) |f| f.frame_id == frame_id else false) else false;
            if (!live) {
                log.debug("tokens: stale recall ack frame {d} from client {x} matched nothing", .{ frame_id, client });
                return 0;
            }
            self.last_ack_ns = clock.monotonicNs();
            n = self.retireFrameLocked(client, self.last_ack_ns);
            stats.dlm_token_recall_acks.add(n);
        }
        self.ack_wake.notifyWaiters(io);
        return n;
    }

    /// `Release{objects}` from `client`: voluntary; answers the count.
    pub fn serveRelease(self: *Holder, io: Io, client: ClientId, objects: []align(1) const Object) u32 {
        var n: u32 = 0;
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            for (objects) |o| n += @intFromBool(self.unregisterLocked(o, client));
        }
        stats.dlm_token_releases.add(n);
        self.ack_wake.notifyWaiters(io);
        return n;
    }

    /// The departure sweep: a member the owner EVICTS or that LEAVES has
    /// its grants retired at that instant — outstanding recalls complete as
    /// `expired_with_lease`, every other grant is `lease_swept`.
    pub fn noteMemberDeparted(self: *Holder, io: Io, client: ClientId) void {
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            self.sweepClientLocked(client, null);
        }
        self.ack_wake.notifyWaiters(io);
    }

    /// The lease sweep at a NEW word from the owner (a joiner's census
    /// commit — a joiner hears no eviction, only the census that no longer
    /// lists the member): every client the oracle now calls expired
    /// against its last contact loses its grants; one with a recall in
    /// flight is left to the pass, woken NOW (its own sweep retires the
    /// frame into its summary) instead of at its next expiry tick.
    /// Answers the clients judged expired.
    pub fn sweepExpired(self: *Holder, io: Io, now_ns: u64) u32 {
        var n: u32 = 0;
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            var expired: std.ArrayListUnmanaged(ClientId) = .empty;
            defer expired.deinit(self.gpa);
            var it = self.clients.iterator();
            while (it.next()) |e| {
                if (self.oracle.verdict(self.oracle.ctx, e.key_ptr.*, e.value_ptr.contact_ns, now_ns) != .expired) continue;
                n += 1;
                if (e.value_ptr.frame == null) expired.append(self.gpa, e.key_ptr.*) catch break;
            }
            for (expired.items) |c| self.sweepClientLocked(c, null);
        }
        if (n != 0) self.ack_wake.notifyWaiters(io);
        return n;
    }

    /// `Grant` (spec §1.3 G3): membership first, register-then-check, a
    /// pending recall of this `(client, object)` waited out, an object in
    /// flight parked to the settle (bounded by the recall deadline), the
    /// records read, a pass that took the object DURING the read re-parks
    /// and re-reads, `Gone` retracts the registration. The page lands in
    /// `scratch` (the session's).
    pub fn serveGrant(self: *Holder, io: Io, ask: GrantAsk, source: RecordSource, scratch: []u8) Error!GrantReply {
        const now = clock.monotonicNs();
        if (self.refuseExpired(ask.client)) return .not_member;
        // The write cache's recall BEFORE the registration: the promotion's
        // own publish runs a pass over the object, which this grant then
        // reads post-commit (the gate's law below).
        if (self.prepare) |p| p.run(p.ctx, io, ask.object);
        const park_deadline = now + self.recallDeadlineNs();
        var attempt: u32 = 0;
        while (true) : (attempt += 1) {
            // A pending recall of this (client, object) is waited out: served
            // on the doomed registration, the token would stand at the
            // reader untracked here.
            while (true) {
                var n = self.ack_wake.notified();
                const pre = n.enable(io);
                const pending = blk: {
                    self.lock.lockUncancelable(io);
                    defer self.lock.unlock(io);
                    break :blk self.pendingRecallLocked(ask.object, ask.client);
                };
                if (!pending) {
                    n.cancel(io);
                    break;
                }
                stats.dlm_token_regrant_under_recall_waits.inc();
                if (pre) {
                    n.cancel(io);
                    continue;
                }
                if (n.waitUntilUncancelable(io, park_deadline) == .deadline) return error.ParkExpired;
            }
            // The membership lease again at every registration: the wait
            // for the member's own recall above ends at its lease's expiry
            // when the member stopped acking.
            if (self.refuseExpired(ask.client)) return .not_member;
            // Register FIRST (the lane's lock), THEN check the gate.
            {
                self.lock.lockUncancelable(io);
                defer self.lock.unlock(io);
                _ = try self.registerLocked(ask.object, ask.client, now);
            }
            if (!try self.awaitSettled(io, ask.object, park_deadline)) {
                self.retract(io, ask.object, ask.client);
                stats.dlm_token_grant_park_refusals.inc();
                return error.ParkExpired;
            }
            self.holdIfArmed(io, self.seams.grant_hold_after_check);
            const built = self.buildPage(ask, source, scratch) catch |e| {
                self.retract(io, ask.object, ask.client);
                return e;
            };
            const page = built orelse {
                self.retract(io, ask.object, ask.client);
                return .gone;
            };
            // A pass that took the object in flight DURING the read: the
            // records may be pre-commit — re-park and re-read.
            if (self.gate.admission(io, ask.object) == .park) continue;
            // A recall landed inside the read: the registration is doomed;
            // repeat (the wait above drains it).
            const recalled = blk: {
                self.lock.lockUncancelable(io);
                defer self.lock.unlock(io);
                break :blk self.pendingRecallLocked(ask.object, ask.client);
            };
            if (recalled) continue;
            stats.dlm_token_grants_served.inc();
            return .{ .granted = page };
        }
    }

    /// The membership lease FIRST (the frame law): a client the owner lists
    /// EXPIRED is refused `NotMember` and counted, nothing registered.
    fn refuseExpired(self: *Holder, client: ClientId) bool {
        const now = clock.monotonicNs();
        if (self.oracle.verdict(self.oracle.ctx, client, now, now) != .expired) return false;
        stats.dlm_token_nonmember_refusals.inc();
        log.warn("tokens: a grant ask from client {x} refused NotMember — the membership owner lists its lease EXPIRED (a frame it sent while live, served after its deadline)", .{client});
        return true;
    }

    fn retract(self: *Holder, io: Io, object: Object, client: ClientId) void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        _ = self.unregisterLocked(object, client);
    }

    /// Park while the object is in flight; false past the deadline.
    fn awaitSettled(self: *Holder, io: Io, object: Object, deadline_ns: u64) Error!bool {
        var parked = false;
        while (true) {
            var n = self.gate.settled.notified();
            const pre = n.enable(io);
            if (self.gate.admission(io, object) == .proceed) {
                n.cancel(io);
                return true;
            }
            if (!parked) {
                parked = true;
                stats.dlm_token_grant_parks.inc();
            }
            if (pre) {
                n.cancel(io);
                continue;
            }
            if (n.waitUntilUncancelable(io, deadline_ns) == .deadline) return false;
        }
    }

    /// ONE page under the budget: the folded inode, the carried xattrs
    /// strictly after `xattr_after` in name order, then — only once the
    /// xattr set is complete — the dentries strictly after cookie `after`.
    /// Null = the object is absent.
    fn buildPage(self: *Holder, ask: GrantAsk, source: RecordSource, scratch: []u8) Error!?Page {
        const v = (try source.inode(source.ctx, ask.object)) orelse return null;
        var arena_state = std.heap.ArenaAllocator.init(self.gpa);
        defer arena_state.deinit();
        const arena = arena_state.allocator();
        const half = scratch.len / 2;
        var xb: codec.Builder = .init(scratch[0..half]);
        var db: codec.Builder = .init(scratch[half..]);
        var budget: usize = @min(GRANT_PAGE_BUDGET, half);
        var flags: u8 = 0;
        var n_xattrs: u32 = 0;
        var n_dir: u32 = 0;
        var xattrs_complete = !ask.wants_records;
        if (ask.wants_records) {
            var xs: std.ArrayList(XattrRec) = .empty;
            try source.xattrs(source.ctx, arena, ask.object, &xs);
            xattrs_complete = true;
            for (xs.items) |x| {
                if (!isCarriedXattr(x.name)) continue;
                if (ask.xattr_after.len != 0 and std.mem.order(u8, x.name, ask.xattr_after) != .gt) continue;
                const cost = encodedXattrLen(x);
                if (cost > budget) {
                    xattrs_complete = false;
                    break;
                }
                verbs.GrantPage.encodeXattr(&xb, .{ .name = x.name, .value = x.value }) catch return error.Corrupt;
                budget -= cost;
                n_xattrs += 1;
            }
        }
        if (xattrs_complete) flags |= verbs.PAGE_XATTRS_COMPLETE;
        if (ask.wants_dentries and xattrs_complete) {
            flags |= verbs.PAGE_HAS_DIR;
            var ds: std.ArrayList(DirRec) = .empty;
            // Ask for one page's worth at the 24 B floor; the byte budget
            // decides what fits.
            const max = @max(budget / @as(usize, DIR_RECORD_BYTES), 1);
            const more = try source.dentries(source.ctx, arena, ask.object, ask.after, max, &ds);
            var complete = !more;
            for (ds.items) |d| {
                const cost = @as(usize, DIR_RECORD_BYTES) + d.name.len;
                if (cost > budget or d.name.len > record.NAME_MAX) {
                    complete = false;
                    break;
                }
                verbs.GrantPage.encodeDir(&db, .{ .head = .{ .cookie = d.cookie, .child_ino = d.child_ino, .file_type = d.file_type, .name_len = @intCast(d.name.len), .reserved = @splat(0) }, .name = d.name }) catch return error.Corrupt;
                budget -= cost;
                n_dir += 1;
            }
            if (complete) flags |= verbs.PAGE_DIR_COMPLETE;
        }
        const attrs: verbs.InodeValueWire = std.mem.bytesToValue(verbs.InodeValueWire, &v.encode());
        return .{
            .head = .{ .kind = @backingInt(verbs.GrantPageKind.granted), .flags = flags, .reserved = 0, .n_xattrs = n_xattrs, .n_dir = n_dir, .holder = 0, .attrs = attrs, .reserved2 = @splat(0) },
            .xattrs_bytes = xb.written(),
            .dir_bytes = db.written(),
        };
    }

    /// The encoded cost of one xattr on the wire (head + bytes + padding).
    fn encodedXattrLen(x: XattrRec) usize {
        const raw = x.name.len + x.value.len;
        return codec.wireSize(verbs.XattrHead) + raw + (8 - raw % 8) % 8;
    }

    // ---- the test holds ----

    fn holdIfArmed(self: *Holder, io: Io, armed: bool) void {
        if (!armed) return;
        _ = self.holding.fetchAdd(1, .seq_cst);
        defer _ = self.holding.fetchSub(1, .seq_cst);
        while (self.hold.load(.seq_cst) == 0) {
            var n = self.hold_wake.notified();
            const pre = n.enable(io);
            if (self.hold.load(.seq_cst) != 0) {
                n.cancel(io);
                break;
            }
            if (!pre) n.waitUncancelable(io) else n.cancel(io);
        }
    }

    /// Release every armed hold (idempotent).
    pub fn releaseHold(self: *Holder, io: Io) void {
        self.hold.store(1, .seq_cst);
        self.hold_wake.notifyWaiters(io);
    }

    /// Someone is standing in a hold (the test's observable).
    pub fn isHolding(self: *const Holder) bool {
        return self.holding.load(.seq_cst) != 0;
    }

    // ---- observables ----

    pub fn grantsOf(self: *Holder, io: Io, object: Object) u32 {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const h = self.objects.getPtr(object) orelse return 0;
        return @intCast(h.list.items.len);
    }

    pub fn holdsGrant(self: *Holder, io: Io, object: Object, client: ClientId) bool {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const h = self.objects.getPtr(object) orelse return false;
        return h.find(client) != null;
    }

    /// The pin's end of `client`'s grant on `object` (0 = live, null = not
    /// registered).
    pub fn recalledUntil(self: *Holder, io: Io, object: Object, client: ClientId) ?u64 {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const h = self.objects.getPtr(object) orelse return null;
        const i = h.find(client) orelse return null;
        return h.list.items[i].recalled_until_ns;
    }

    pub fn outstandingGrants(self: *Holder, io: Io) u64 {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        var n: u64 = 0;
        var it = self.objects.valueIterator();
        while (it.next()) |h| n += h.list.items.len;
        return n;
    }
};

// ---------------------------------------------------------------------------
// Tests: the pure laws (the protocol contracts ride `tokens.zig` over the
// loopback hub).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "recordObjectIno: every ino-major kind names the leading ino, refs their owner, tree 0 nothing; the union sorts, dedupes and skips token_quiet members" {
    try testing.expectEqual(@as(?Object, 77), recordObjectIno(key.TREE_INODES, &key.inodeKey(77)));
    try testing.expectEqual(@as(?Object, 5), recordObjectIno(key.TREE_DENTRIES, &key.dentryKey(5, 123, 0)));
    try testing.expectEqual(@as(?Object, 9), recordObjectIno(key.TREE_XATTRS, &key.xattrKey(9, 1, 0)));
    try testing.expectEqual(@as(?Object, null), recordObjectIno(key.TREE_CONTROL, "slot_state:1"));
    try testing.expectEqual(@as(?Object, null), recordObjectIno(key.TREE_INODES, "short"));
    const gpa = testing.allocator;
    var a = tx.KvTx.init(gpa, @src());
    try a.stagePut(key.TREE_INODES, &key.inodeKey(9), "v");
    try a.stagePut(key.TREE_DENTRIES, &key.dentryKey(5, 1, 0), "d");
    try a.stagePut(key.TREE_INODES, &key.inodeKey(5), "v");
    var b = tx.KvTx.init(gpa, @src());
    try b.stagePut(key.TREE_XATTRS, &key.xattrKey(1, 1, 0), "x");
    b.token_quiet = true;
    var qa = try tx.buildQueuedTx(&a, 65536, null);
    defer qa.deinit();
    var qb = try tx.buildQueuedTx(&b, 65536, null);
    defer qb.deinit();
    var u: ObjectUnion = .{};
    defer u.deinit(gpa);
    try u.addMember(gpa, &qa);
    try u.addMember(gpa, &qb);
    try testing.expectEqualSlices(Object, &.{ 5, 9 }, u.finish());
}

test "an access stamp recalls no reader: a Δ naming the atime alone leaves its object out of the union; a Δ that moves the atime beside mtime/ctime, a size Δ and a whole inode put still recall" {
    const gpa = testing.allocator;
    var a = tx.KvTx.init(gpa, @src());
    try a.stageDelta(key.TREE_INODES, &key.inodeKey(3), .{ .mask = record.DELTA_ATIME, .fields = .{ .atime = 7 } });
    try a.stageDelta(key.TREE_INODES, &key.inodeKey(4), .{ .mask = record.DELTA_ATIME | record.DELTA_TIMES, .fields = .{ .atime = 7, .mtime = 7, .ctime = 7 } });
    try a.stageDelta(key.TREE_INODES, &key.inodeKey(5), .{ .mask = record.DELTA_SIZE, .fields = .{ .size = 9 } });
    try a.stagePut(key.TREE_INODES, &key.inodeKey(6), "v");
    var qa = try tx.buildQueuedTx(&a, 65536, null);
    defer qa.deinit();
    var u: ObjectUnion = .{};
    defer u.deinit(gpa);
    try u.addMember(gpa, &qa);
    try testing.expectEqualSlices(Object, &.{ 4, 5, 6 }, u.finish());
}

test "the carried-xattr law: the allowlist, layout and the symlink target are tokened; the control plane's names are not" {
    try testing.expect(isCarriedXattr("user.a") and isCarriedXattr("layout") and isCarriedXattr("system.symlink") and isCarriedXattr("security.selinux") and isCarriedXattr("trusted.x"));
    try testing.expect(!isCarriedXattr("writer_claim") and !isCarriedXattr("client:abc") and !isCarriedXattr("job:enroll") and !isCarriedXattr("user.squeezefs.gather") and !isCarriedXattr("system.posix_acl_access"));
}

test "constants with their WHY: the deadline is T_owner, the tick its quarter, the page budget half the control cap, the fanout buckets the group-size law" {
    try testing.expectEqual(@as(u64, 45_000 * ns_per_ms), RECALL_DEADLINE_NS);
    try testing.expectEqual(EXPIRY_TICK_FLOOR_NS, expiryTickNs(1));
    try testing.expectEqual(@as(usize, 0), fanoutBucket(1));
    try testing.expectEqual(@as(usize, 7), fanoutBucket(8));
    try testing.expectEqual(@as(usize, 10), fanoutBucket(64));
    try testing.expectEqual(@as(usize, 11), fanoutBucket(12_500));
}

test "the gate is a refcount: two users keep an object in flight until the LAST settles; a settle of an unmarked object is a no-op" {
    const io = testing.io;
    var g = GrantPassGate.init(testing.allocator);
    defer g.deinit();
    const objs = [_]Object{ 1, 2 };
    try g.passBegin(io, &objs);
    try g.passBegin(io, objs[1..]);
    try testing.expectEqual(Admission.park, g.admission(io, 2));
    g.settle(io, &objs);
    try testing.expectEqual(Admission.proceed, g.admission(io, 1));
    try testing.expectEqual(Admission.park, g.admission(io, 2));
    g.settle(io, objs[1..]);
    try testing.expectEqual(Admission.proceed, g.admission(io, 2));
    g.settle(io, &.{99});
    try testing.expectEqual(@as(usize, 0), g.inflightCount(io));
}
