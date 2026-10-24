//! The `-o ro` reader's plane (Linux; charter §5.5 "`-o ro` mounts are
//! member-readers + token clients (every reader TTL derives to 0;
//! `reader_staleness_bound_ms` reads 0); with NO live writer a `-o ro`
//! mount is the QUIESCENT reader of the checkpointed projection that
//! fail-stops `ESTALE` at the first poll that sees a writer";
//! `tokens/ro_mount.zig`): what `mount -o ro` of a symmetric set arms
//! over its READER backends (`backend.KvBackend.openReader`: the
//! projection, no flock, no claim, no ring).
//!
//! The arm is decided at the mount by the writer faces — an exclusive
//! writer `flock` on the device on this host (read off `/proc/locks`, no
//! lock of our own) or a heartbeat-fresh `writer_claim` on any volume:
//!
//! - TOKENS: a member of the home shard as a READER (a random 16 B member
//!   id, the role `reader`, no region, no park gate), the manager link off
//!   tree 0's `endpoint:{0}` record (re-resolved before every dial), ONE
//!   listener for the pushed recalls, and the divert with every slot
//!   FOREIGN — a slot tree 0 leases is its lessee's, an unleased one the
//!   manager's (it maintains every unleased tree) — so every object is
//!   read under a token from its holder: 0 device reads of slot trees, the
//!   holder's recall the freshness proof. The lease expiring (the manager
//!   gone) re-evaluates the faces at the beat: no writer left ⇒ the
//!   quiescent posture below; a writer stands ⇒ the shard is rejoined
//!   (the planes re-pointed) while every serve answers the typed EAGAIN.
//! - QUIESCENT: the projection served off the device exactly (nothing
//!   writes); the S5 epoch poll of the fixed ledger and the flock face at
//!   the checkpoint landing ceiling — either seeing a writer fail-stops
//!   the mount ONCE (`ro_mount.QuiescentReader`): every metadata op
//!   answers ESTALE through `fuse/handler/reader_gate.zig`, `.stats` the
//!   exception; the remedy is a remount (which arms as a token reader).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const errors = @import("../../core/errors.zig");
const thread_name = @import("../../core/thread_name.zig");
const key = @import("../key.zig");
const backend = @import("../backend.zig");
const meta_set = @import("../meta_set.zig");
const claim = @import("../guard/claim.zig");
const lease_clock = @import("../guard/lease_clock.zig");
const guard_gate = @import("../guard/gate.zig");
const proc = @import("../guard/proc.zig");
const cr = @import("control_records.zig");
const joined = @import("joined.zig");
const membership = @import("membership.zig");
const shard = membership.shard;
const join_ladder = membership.join_ladder;
const park = membership.park_reclaim;
const tokens = @import("tokens.zig");
const ro = tokens.ro;
const wire_plane = @import("wire_plane.zig");
const wire_auth = @import("../../wire/auth.zig");
const listener = @import("../../wire/listener.zig");
const endpoint = @import("../../wire/endpoint.zig");
const service = @import("../../wire/service.zig");
const verbs = @import("../../wire/verbs.zig");
const page = @import("appender_page.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("sym/reader_plane is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const MetaSet = meta_set.MetaSet;
const KvBackend = backend.KvBackend;

pub const stats = struct {
    /// Readers that armed under tokens / quiescent at the mount.
    pub var reader_token_arms: counters.ShardedCounter = .{};
    pub var reader_quiescent_arms: counters.ShardedCounter = .{};
    /// A token reader whose writers left: the quiescent posture adopted.
    pub var reader_quiescent_transitions: counters.ShardedCounter = .{};
    /// A token reader whose lease lapsed under a standing writer rejoined.
    pub var reader_rejoins: counters.ShardedCounter = .{};
    /// Polls of the quiescent reader (the epoch poll + the flock face).
    pub var reader_quiescent_polls: counters.ShardedCounter = .{};
};

pub const StartError = error{
    /// `SQUEEZEFS_MW_BIND` is `off`: a token reader has no listener for
    /// the recalls, so a live writer's set cannot be read.
    MembershipOff,
    MalformedBind,
    EnrollMissing,
    ListenerRefused,
    /// No manager endpoint on tree 0 while a writer is live.
    ManagerUnreachable,
    /// The membership join was refused.
    JoinRefused,
    Io,
    OutOfMemory,
};

pub fn startReason(e: StartError) []const u8 {
    return switch (e) {
        error.MembershipOff => "SQUEEZEFS_MW_BIND=off: a `-o ro` reader of a LIVE set is a token client and needs a listener for the holders' recalls",
        error.MalformedBind => "SQUEEZEFS_MW_BIND is neither `auto`, `off` nor an `addr:port` / `[v6]:port` literal",
        error.EnrollMissing => "volume 0's control tree carries no decodable job:enroll record — formatted before the cluster wire existed; reformat",
        error.ListenerRefused => "the reader's listener could not bind (see the log)",
        error.ManagerUnreachable => "a writer holds the set but no manager endpoint is published on tree 0",
        error.JoinRefused => "the home shard refused the reader's membership join (see the log)",
        error.Io => "device I/O failed",
        error.OutOfMemory => "out of memory",
    };
}

/// The beat: a tenth of the shipped `T_self` floor (the joined half's
/// cadence) — the member's renewals and the quiescent poll's clock.
const BEAT_MS: u64 = 80;
/// The quiescent poll's cadence: the checkpoint landing ceiling — a
/// writer's first checkpoint lands within it, and its claim/flock are
/// older still, so a writer is seen within ONE ceiling of its mount.
const QUIESCENT_POLL_MS: u64 = shard.CHECKPOINT_LANDING_CEILING_MS;
/// The lease table's slots: the reader resolves holders for every
/// routing slot (the lease map is a RAM array, 4 B per slot).
const SLOTS: usize = @import("../routing/route_table.zig").W;

comptime {
    std.debug.assert(BEAT_MS * 10 < QUIESCENT_POLL_MS);
}

/// A holder that leases no slot: the manager (it maintains every
/// unleased tree itself).
const MANAGER_HOLDER: u32 = @import("../journal.zig").FIXED_RING_APPENDER;

pub const ReaderPlane = struct {
    gpa: Allocator,
    io: Io,
    set: *MetaSet,
    env: *const std.process.Environ.Map,
    mountpoint: []const u8,
    svc: service.Service = .init,
    listener: ?*listener.Listener = null,
    advertised: endpoint.EndpointWire = .none,
    endpoint_buf: [wire_plane.ENDPOINT_TEXT_MAX]u8 = undefined,
    endpoint_len: usize = 0,
    /// This reader's wire identity (the dial's peer id; the node token
    /// this host's, the writer id random: a reader holds no region).
    identity: verbs.WireIdentity,
    member_id: shard.MemberId,
    secret: wire_auth.Secret,
    link: joined.SharedLink,
    manager_resolver: joined.ManagerResolver,
    wire: join_ladder.ManagerWire,
    member: shard.Member,
    lease: ro.MemberLease = .{},
    /// The arm in force (`tokens` | `quiescent`), read by the divert's
    /// resolver on every serve (acquire; the transition's release).
    arm: std.atomic.Value(u8),
    quiescent: ro.QuiescentReader,
    poll: ro.EpochPoll,
    /// Slot → lessee appender id (0 = unleased: the manager's), primed off
    /// tree 0 at the arm, learnt from redirects after.
    leases: []std.atomic.Value(u32),
    holders: HolderLinks,
    divert: tokens.Divert,
    reader_svc: tokens.divert.ReaderService,
    sink: wire_plane.RecallSink = .{},
    poison: ReaderPoison = .{},
    beat: ?std.Thread = null,
    stopping: std.atomic.Value(bool) = .init(false),
    last_poll_ns: u64 = 0,
    renewals: std.atomic.Value(u64) = .init(0),

    /// Arm the plane (module doc): the writer faces decide tokens or
    /// quiescent. `mountpoint` is the member row's.
    pub fn start(gpa: Allocator, io: Io, set: *MetaSet, env: *const std.process.Environ.Map, raw_cpus: u32, mem_budget_bytes: u64, mountpoint: []const u8) StartError!*ReaderPlane {
        const be0 = set.volumes[0];
        std.debug.assert(be0.posture == .reader);
        const secret = joined.readControl(gpa, be0, wire_auth.ENROLL_KEY, wire_auth.Secret, wire_auth.decodeEnroll) catch |e| return mapOpen(e);
        const local = proc.localIdentity();
        var random: [16]u8 = undefined;
        io.randomSecure(&random) catch return error.Io;
        const identity: verbs.WireIdentity = .{ .node_token = @import("join.zig").nodeToken(local), .writer_id = std.mem.readInt(u64, random[0..8], .little), .mount_slot = joined.joinerMountSlot(mountpoint), .reserved = 0 };
        const self = try gpa.create(ReaderPlane);
        errdefer gpa.destroy(self);
        self.* = .{
            .gpa = gpa,
            .io = io,
            .set = set,
            .env = env,
            .mountpoint = mountpoint,
            .identity = identity,
            .member_id = random,
            .secret = secret orelse return error.EnrollMissing,
            .link = undefined,
            .manager_resolver = undefined,
            .wire = undefined,
            .member = undefined,
            .arm = .init(@backingInt(ro.ReaderArm.quiescent)),
            .quiescent = ro.QuiescentReader.init(be0.mounted.seq),
            .poll = undefined,
            .leases = undefined,
            .holders = .{ .plane = self },
            .divert = undefined,
            .reader_svc = undefined,
        };
        self.poll = ro.EpochPoll.init(gpa, .{ .ctx = be0, .readSlot = &joined.ManagerResolver.readSlot, .readAll = &joined.ManagerResolver.readAll }, be0.mounted.seq);
        self.leases = try gpa.alloc(std.atomic.Value(u32), SLOTS);
        errdefer gpa.free(self.leases);
        for (self.leases) |*l| l.* = .init(MANAGER_HOLDER);
        // The manager link: tree 0's record (or none yet: the quiescent
        // arm needs no manager).
        const ep = joined.readControl(gpa, be0, cr.endpointKey(0).slice(), endpoint.EndpointWire, joined.decodeEndpoint) catch |e| return mapOpen(e);
        self.link = joined.SharedLink.init(gpa, io, ep orelse .none, self.secret, identity);
        errdefer self.link.deinit();
        self.manager_resolver = joined.ManagerResolver.init(be0, &self.link);
        self.link.resolver = self.manager_resolver.face();
        self.wire = .{ .gpa = gpa, .link = self.link.link(), .volume = 0, .identity = identity, .pages = .{ .ctx = self, .read = &readPageFn } };
        self.divert = tokens.Divert.init(gpa, io, self.resolver(), wire_plane.clientIdOf(self.member_id), 0, self.sink.sink(), self.lease.selfLease(), tokens.cache.recordsBudgetBytes(mem_budget_bytes));
        errdefer self.divert.deinit();
        self.reader_svc = .{ .io = io, .divert = &self.divert };
        ro.stats.reader_staleness_bound_ms.set(ro.READER_STALENESS_BOUND_MS);
        const live_writer = self.writerLive();
        switch (ro.armFor(live_writer)) {
            .tokens => try self.armTokens(raw_cpus),
            .quiescent => self.armQuiescent(),
        }
        self.beat = std.Thread.spawn(.{}, run, .{self}) catch return error.OutOfMemory;
        return self;
    }

    fn mapOpen(e: joined.OpenError) StartError {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.EnrollMissing,
            else => error.Io,
        };
    }

    /// The faces of a live writer: an exclusive flock on any volume of
    /// this host; a heartbeat-fresh writer claim on any volume.
    fn writerLive(self: *ReaderPlane) bool {
        for (self.set.volumes) |be| {
            if (proc.writerFlockHeld(self.io, be.path, null)) return true;
            const raw = guard_gate.probeClaim(self.gpa, self.io, self.env, self.set.pool, be.path) catch continue;
            defer if (raw) |r| self.gpa.free(r);
            const c = claim.Claim.decode(raw orelse continue) catch continue;
            if (claim.Registration.fromClaim(c, proc.localIdentity(), clock.realtimeCoarseNs(), proc.probe).state == .live) return true;
        }
        return false;
    }

    // ---- the token arm ----

    fn armTokens(self: *ReaderPlane, raw_cpus: u32) StartError!void {
        const be0 = self.set.volumes[0];
        const bind = listener.bindFromEnv(self.env) catch return error.MalformedBind;
        if (bind == .off) return error.MembershipOff;
        if (self.link.endpointNow().isNone()) {
            const ep = joined.awaitManagerEndpoint(self.gpa, be0) catch |e| return switch (e) {
                error.ManagerUnreachable => error.ManagerUnreachable,
                else => mapOpen(e),
            };
            self.link.ep = ep;
        }
        try self.primeLeases();
        if (self.listener == null) {
            self.reader_svc.register(&self.svc);
            const l = listener.Listener.start(self.gpa, self.io, bind, &self.svc, self.secret, .{ .env = self.env, .raw_cpus = raw_cpus, .nofile = listener.raiseNofile() }) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => {
                    log.err("reader plane: the listener could not start ({s})", .{@errorName(e)});
                    return error.ListenerRefused;
                },
            };
            self.listener = l;
            self.advertised = l.advertised;
            var w: Io.Writer = .fixed(&self.endpoint_buf);
            self.advertised.format(&w) catch {};
            self.endpoint_len = w.buffered().len;
        }
        self.member = .{
            .gpa = self.gpa,
            .id = self.member_id,
            .role = .reader,
            .link = self.link.link(),
            .pid = @intCast(linux.getpid()),
            .boot = proc.localIdentity().boot_id,
            .endpoint = self.advertised,
            .pr_key = 0,
            .mount = self.mountpoint,
            .gate = null,
            .poison = self.poison.sink(),
            .carriage = null,
        };
        const now = nowMs();
        switch (self.member.join(now, null)) {
            .granted => {},
            else => |v| {
                log.err("reader plane: the home shard refused the reader's join ({t})", .{v});
                return error.JoinRefused;
            },
        }
        self.lease.renew(clock.monotonicNs(), self.member.clocks.?.t_owner_ms * std.time.ns_per_ms);
        self.arm.store(@backingInt(ro.ReaderArm.tokens), .release);
        ro.stats.reader_quiescent.set(0);
        stats.reader_token_arms.inc();
        log.info("reader plane: `-o ro` of a LIVE set — a member-reader of the home shard at {f} (epoch {d}, T_owner {d} ms), listening at {s}; every object read under a token from its holder, every TTL 0", .{ self.link.endpointNow(), self.member.epoch, self.member.clocks.?.t_owner_ms, self.endpointText() });
    }

    /// Tree 0's `slot_state` records into the lease map (every leased slot
    /// its lessee's; the rest the manager's).
    fn primeLeases(self: *ReaderPlane) StartError!void {
        const be0 = self.set.volumes[0];
        var kvs: std.ArrayList(@import("../tree.zig").KeyValue) = .empty;
        defer @import("../tree.zig").deinitKeyValues(self.gpa, &kvs);
        const r = cr.rangeOf(.slot_state);
        be0.range(self.gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        for (self.leases) |*l| l.store(MANAGER_HOLDER, .release);
        for (kvs.items) |kv| {
            const slot = cr.slotOfKey(kv.key) orelse continue;
            const s = cr.SlotStateWire.decode(kv.value) catch return error.Io;
            if (s.variant != .unleased) self.leases[slot].store(s.appender_id, .release);
        }
    }

    // ---- the quiescent arm ----

    fn armQuiescent(self: *ReaderPlane) void {
        const be0 = self.set.volumes[0];
        self.quiescent = ro.QuiescentReader.init(be0.mounted.seq);
        self.poll = ro.EpochPoll.init(self.gpa, .{ .ctx = be0, .readSlot = &joined.ManagerResolver.readSlot, .readAll = &joined.ManagerResolver.readAll }, be0.mounted.seq);
        self.last_poll_ns = clock.monotonicNs();
        self.arm.store(@backingInt(ro.ReaderArm.quiescent), .release);
        stats.reader_quiescent_arms.inc();
        log.info("reader plane: `-o ro` with NO live writer — the QUIESCENT reader of the checkpointed projection (seq {d}); a writer seen by the poll fail-stops every op ESTALE", .{be0.mounted.seq});
    }

    /// The token reader whose writers all LEFT: every volume's projection
    /// re-pointed at the writers' last checkpoint, the arm flipped (every
    /// serve local from here), the poll armed at the adopted seq.
    fn becomeQuiescent(self: *ReaderPlane) void {
        for (self.set.volumes) |be| _ = be.adoptNewestCheckpoint() catch |e| log.err("reader plane: the projection of {s} could not adopt the newest checkpoint ({s}) — the mount-time projection stands", .{ be.path, @errorName(e) });
        self.armQuiescent();
        stats.reader_quiescent_transitions.inc();
        log.info("reader plane: every writer left — the token reader became the quiescent reader", .{});
    }

    /// One quiescent poll: the ledger's predicted slot and the flock face.
    fn pollQuiescent(self: *ReaderPlane) void {
        stats.reader_quiescent_polls.inc();
        const be0 = self.set.volumes[0];
        var face: ro.WriterFace = .none;
        var out = self.poll.poll() catch return;
        if (out.newest) |*rec| {
            rec.deinit(self.gpa);
            face = .ledger_advance;
        }
        if (face == .none and proc.writerFlockHeld(self.io, be0.path, null)) face = .same_host_lock;
        self.quiescent.onPoll(face, self.mountpoint);
    }

    // ---- the beat ----

    fn run(self: *ReaderPlane) void {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, "sqz-reader");
        while (!self.stopping.load(.acquire)) {
            const t = clock.monotonicNs();
            switch (self.armNow()) {
                .tokens => {
                    const v = self.member.tick(t / std.time.ns_per_ms, t);
                    switch (v) {
                        .renewed => {
                            self.lease.renew(t, self.member.clocks.?.t_owner_ms * std.time.ns_per_ms);
                            _ = self.renewals.fetchAdd(1, .monotonic);
                        },
                        .fenced => self.reevaluate(),
                        else => {},
                    }
                },
                .quiescent => if (t -| self.last_poll_ns >= QUIESCENT_POLL_MS * std.time.ns_per_ms) {
                    self.last_poll_ns = t;
                    self.pollQuiescent();
                },
            }
            const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(BEAT_MS * std.time.ns_per_ms) };
            _ = linux.nanosleep(&ts, null);
        }
    }

    /// The lease lapsed (`T_self` with the manager unreachable): the
    /// faces decide — no writer ⇒ quiescent; a writer ⇒ rejoin (the
    /// planes re-pointed), every serve the typed EAGAIN meanwhile.
    fn reevaluate(self: *ReaderPlane) void {
        if (!self.writerLive()) {
            self.becomeQuiescent();
            return;
        }
        self.member.words.fenced.store(false, .release);
        switch (self.member.join(nowMs(), null)) {
            .granted => {
                self.lease.renew(clock.monotonicNs(), self.member.clocks.?.t_owner_ms * std.time.ns_per_ms);
                self.primeLeases() catch {};
                _ = self.divert.repoint(self.io, MANAGER_HOLDER, MANAGER_HOLDER);
                stats.reader_rejoins.inc();
                log.info("reader plane: the home shard at {f} re-admitted the reader (epoch {d}) — the manager's token plane re-pointed", .{ self.link.endpointNow(), self.member.epoch });
            },
            else => {},
        }
    }

    fn armNow(self: *const ReaderPlane) ro.ReaderArm {
        return @fromBackingInt(@intCast(self.arm.load(.acquire)));
    }

    pub fn isQuiescent(self: *const ReaderPlane) bool {
        return self.armNow() == .quiescent;
    }

    pub fn endpointText(self: *const ReaderPlane) []const u8 {
        return self.endpoint_buf[0..self.endpoint_len];
    }

    /// The move gate's bound (decision 0003 §X.1): `T_park_max` at the
    /// membership clocks a live set granted this reader (the shipped
    /// `T_owner` before any grant — a quiescent reader's).
    pub fn moveDeadlineMs(self: *const ReaderPlane) u64 {
        return park.tParkMaxMs(@import("extent_grant.zig").failoverBoundMs(0, 0), park.graceMs(self.tOwnerMs()));
    }

    /// The home shard's `T_owner` this member reads by (the constant before
    /// its grant).
    pub fn tOwnerMs(self: *const ReaderPlane) u64 {
        return if (self.member.clocks) |c| c.t_owner_ms else lease_clock.T_OWNER_MS;
    }

    /// The fail-stop gate the FUSE layer wraps the filesystem with.
    pub fn gate(self: *ReaderPlane) @import("../../fuse/handler/reader_gate.zig").Gate {
        return .{ .ctx = self, .failStopped = &failStoppedFn };
    }

    fn failStoppedFn(ctx: *anyopaque) bool {
        const self: *ReaderPlane = @ptrCast(@alignCast(ctx));
        return self.quiescent.isFailStopped();
    }

    /// A foreign file's layout record off its holder's token (the data
    /// plane's face); local on the quiescent reader.
    pub fn foreignLayout(self: *ReaderPlane) @import("../../data/plane.zig").DataPlane.ForeignLayout {
        return .{ .ctx = self, .lookup = &foreignLayoutFn, .mapRecord = &foreignMapRecordFn };
    }

    fn foreignLayoutFn(ctx: *anyopaque, gpa: Allocator, ino: u64) errors.Error!@import("../../data/plane.zig").DataPlane.ForeignLayout.Verdict {
        const self: *ReaderPlane = @ptrCast(@alignCast(ctx));
        return wire_plane.foreignLayoutOf(&self.divert, self.io, gpa, ino);
    }

    fn foreignMapRecordFn(ctx: *anyopaque, gpa: Allocator, ino: u64, block: u32) errors.Error!@import("../../data/plane.zig").DataPlane.ForeignLayout.MapVerdict {
        const self: *ReaderPlane = @ptrCast(@alignCast(ctx));
        return wire_plane.foreignMapRecordOf(&self.divert, self.io, gpa, ino, block);
    }

    pub fn stop(self: *ReaderPlane) void {
        self.stopping.store(true, .release);
        if (self.beat) |b| b.join();
        if (self.armNow() == .tokens) self.member.leave() catch {};
        if (self.listener) |l| l.stop();
        self.divert.deinit();
        self.holders.deinit();
        self.link.deinit();
        self.gpa.free(self.leases);
        self.gpa.destroy(self);
    }

    /// The `.stats` family words.
    pub fn writeStatsJson(self: *ReaderPlane, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"wire_posture\":\"reader\",\"reader_arm\":\"{t}\",\"reader_fail_stopped\":{},\"wire_endpoint\":\"{s}\",\"manager_endpoint\":\"{f}\",\"manager_repoints\":{d},\"member_epoch\":{d},\"member_renewals\":{d},\"reader_holder_links\":{d}}}", .{ self.armNow(), self.quiescent.isFailStopped(), self.endpointText(), self.link.endpointNow(), self.manager_resolver.repoints.load(.monotonic), if (self.armNow() == .tokens) self.member.epoch else 0, self.renewals.load(.monotonic), self.holders.count() });
    }

    fn readPageFn(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self: *ReaderPlane = @ptrCast(@alignCast(ctx));
        const be0 = self.set.volumes[0];
        const img = be0.dev.readRange(self.gpa, offset, page.PAGE_LEN) catch return error.Io;
        defer self.gpa.free(img);
        @memcpy(out, img[0..page.PAGE_LEN]);
    }

    // ---- the divert's resolver: every slot foreign under tokens ----

    fn resolver(self: *ReaderPlane) tokens.divert.Resolver {
        return .{ .ctx = self, .homeOf = &homeOf, .refresh = &refresh, .dial = &dial };
    }

    fn homeOf(ctx: *anyopaque, slot: u16) tokens.divert.Home {
        const self: *ReaderPlane = @ptrCast(@alignCast(ctx));
        if (self.armNow() == .quiescent) return .local;
        const holder = self.leases[slot].load(.acquire);
        return .{ .foreign = .{ .holder = holder, .identity = holder } };
    }

    /// `NotHolder{current}`: the holder's word learnt, followed once.
    fn refresh(ctx: *anyopaque, slot: u16, current: u32) tokens.divert.Home {
        const self: *ReaderPlane = @ptrCast(@alignCast(ctx));
        self.leases[slot].store(current, .release);
        return homeOf(ctx, slot);
    }

    fn dial(ctx: *anyopaque, _: tokens.table.ClientId, holder: u32) ?tokens.cache.Link {
        const self: *ReaderPlane = @ptrCast(@alignCast(ctx));
        if (holder == MANAGER_HOLDER) return self.link.tokenLink();
        return self.holders.linkFor(holder);
    }

    fn nowMs() u64 {
        return clock.monotonicNs() / std.time.ns_per_ms;
    }
};

/// The reader's poison sink: a reader has no region to fence and no
/// volume to latch FAILED — the lapse is the beat's to re-evaluate.
const ReaderPoison = struct {
    fn poison(_: ?*anyopaque, reason: []const u8) void {
        log.warn("reader plane: the member lease lapsed ({s}) — re-evaluating the writer faces", .{reason});
    }

    fn sink(self: *ReaderPoison) park.PoisonSink {
        return .{ .ctx = self, .poison = &poison };
    }
};

/// The dialled links to the holders other than the manager, ONE per
/// appender id, each re-resolving its listener through the manager's
/// `ResolveEndpoint` before every dial (a moved or restarted holder is
/// followed; one with no listener is `unreachable_`).
const HolderLinks = struct {
    plane: *ReaderPlane,
    lock: Io.Mutex = .init,
    by_holder: std.AutoHashMapUnmanaged(u32, *Peer) = .empty,

    const Peer = struct {
        links: *HolderLinks,
        holder: u32,
        link: joined.SharedLink,

        fn resolveFn(ctx: *anyopaque) ?endpoint.EndpointWire {
            const self: *Peer = @ptrCast(@alignCast(ctx));
            return self.links.plane.wire.resolveEndpoint(self.holder) catch null;
        }
    };

    fn deinit(self: *HolderLinks) void {
        var it = self.by_holder.valueIterator();
        while (it.next()) |p| {
            p.*.link.deinit();
            self.plane.gpa.destroy(p.*);
        }
        self.by_holder.deinit(self.plane.gpa);
    }

    fn count(self: *HolderLinks) usize {
        self.lock.lockUncancelable(self.plane.io);
        defer self.lock.unlock(self.plane.io);
        return self.by_holder.count();
    }

    fn linkFor(self: *HolderLinks, holder: u32) ?tokens.cache.Link {
        self.lock.lockUncancelable(self.plane.io);
        defer self.lock.unlock(self.plane.io);
        if (self.by_holder.get(holder)) |p| return p.link.tokenLink();
        const ep = (self.plane.wire.resolveEndpoint(holder) catch null) orelse return null;
        const p = self.plane.gpa.create(Peer) catch return null;
        p.* = .{ .links = self, .holder = holder, .link = joined.SharedLink.init(self.plane.gpa, self.plane.io, ep, self.plane.secret, self.plane.identity) };
        p.link.resolver = .{ .ctx = p, .resolve = &Peer.resolveFn };
        self.by_holder.put(self.plane.gpa, holder, p) catch {
            p.link.deinit();
            self.plane.gpa.destroy(p);
            return null;
        };
        return p.link.tokenLink();
    }
};

// ---------------------------------------------------------------------------
// Tests: the pure parts (the two-daemon rows ride the smoke).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the quiescent poll runs once per checkpoint landing ceiling, well above the beat" {
    try testing.expectEqual(@as(u64, 1100), QUIESCENT_POLL_MS);
    try testing.expect(BEAT_MS < QUIESCENT_POLL_MS);
}
