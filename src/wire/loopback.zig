//! The in-process loopback transport: N "daemons" in ONE process talk the
//! cluster wire byte-for-byte — the ladder, the framing, the MAC, the
//! service law — without sockets or threads (`docs/porting/sym_wire_
//! membership_recovery.md` §5.4: the reference's `membership_sim` /
//! `sym_n_daemon_tests` shape). Every frame a `Conn` carries is sealed by
//! the caller's `Framer` and accepted by the callee's, so a hub test
//! exercises exactly the bytes a TCP session would, deterministically and
//! on every host. The N-daemon harness of the later waves (N backends
//! over one file-backed set) composes its managers' services here.
//! Portable.

const std = @import("std");
const Io = std.Io;
const errors = @import("../core/errors.zig");
const frame = @import("frame.zig");
const codec = @import("codec.zig");
const verbs = @import("verbs.zig");
const auth = @import("auth.zig");
const session = @import("session.zig");
const service = @import("service.zig");
const stats = @import("stats.zig");

pub const Daemon = struct {
    service: *service.Service,
    gate: auth.Gate,
    /// The daemon's member identity: its proofs present `identity.peerId()`.
    identity: verbs.WireIdentity,
};

pub const ConnectError = error{ HandshakeRefused, SchemaMismatch, OutOfMemory } || Io.RandomSecureError;

pub const LoopbackHub = struct {
    gpa: std.mem.Allocator,
    io: Io,
    daemons: std.ArrayList(Daemon) = .empty,
    /// A monotone clock the nonce registry judges freshness by (the hub's
    /// tests step it; a real listener reads the monotonic clock).
    now_ms: u64 = 1_000_000,

    pub fn init(gpa: std.mem.Allocator, io: Io) LoopbackHub {
        return .{ .gpa = gpa, .io = io };
    }

    pub fn deinit(self: *LoopbackHub) void {
        for (self.daemons.items) |*d| d.gate.deinit(self.gpa);
        self.daemons.deinit(self.gpa);
    }

    /// Register a daemon: its service, the set secret IT holds (a daemon
    /// with another secret is a stranger the others refuse), its identity.
    pub fn addDaemon(self: *LoopbackHub, svc: *service.Service, secret: auth.Secret, identity: verbs.WireIdentity) std.mem.Allocator.Error!usize {
        const gate = try auth.Gate.init(self.gpa, secret, 8);
        try self.daemons.append(self.gpa, .{ .service = svc, .gate = gate, .identity = identity });
        return self.daemons.items.len - 1;
    }

    /// Dial daemon `to` from daemon `from`: the ladder through the payload
    /// codecs, both framers admitted on success.
    pub fn connect(self: *LoopbackHub, from: usize, to: usize) ConnectError!Conn {
        const caller = &self.daemons.items[from];
        const callee = &self.daemons.items[to];
        const ch = try callee.gate.challenge(self.io, self.now_ms);
        stats.cluster_wire_outstanding_challenges.set(callee.gate.nonces.liveCount());
        const peer_id = caller.identity.peerId();
        const ans = try auth.answer(self.io, &caller.gate.secret, &peer_id, ch, "");
        const verdict = callee.gate.verify(.{ .head = ans.prove_head, .bytes = &peer_id }, "", self.now_ms);
        stats.cluster_wire_outstanding_challenges.set(callee.gate.nonces.liveCount());
        const adm = switch (verdict) {
            .admitted => |a| a,
            .refused => {
                stats.cluster_wire_admissions_refused.inc();
                return error.HandshakeRefused;
            },
        };
        stats.cluster_wire_sessions_admitted.inc();
        var conn: Conn = .{
            .hub = self,
            .to = to,
            .client = .init(.peer),
            .server = .init(.coordinator),
            .peer = adm.member(),
            .frame_buf = try self.gpa.alloc(u8, session.SEND_BUF_LEN),
            .reply_buf = undefined,
            .scratch = undefined,
        };
        errdefer self.gpa.free(conn.frame_buf);
        conn.reply_buf = try self.gpa.alloc(u8, session.SEND_BUF_LEN);
        errdefer self.gpa.free(conn.reply_buf);
        conn.scratch = try self.gpa.alloc(u8, frame.CONTROL_CAP);
        conn.client.admit(ans.session_key);
        conn.server.admit(adm.session_key);
        return conn;
    }
};

/// One dialed in-process session: the caller's framer, the callee's
/// framer, the callee's view of the admitted peer.
pub const Conn = struct {
    hub: *LoopbackHub,
    to: usize,
    client: session.Framer,
    server: session.Framer,
    peer: ?verbs.WireIdentity,
    frame_buf: []u8,
    reply_buf: []u8,
    scratch: []u8,
    next_correlation: u64 = 1,
    /// A framing failure on either side poisons the connection, exactly
    /// as a TCP session's.
    poisoned: bool = false,
    /// Test seam: flip one byte of the next call frame after sealing (the
    /// tamper-in-flight repro).
    tamper_next: bool = false,

    pub fn deinit(self: *Conn) void {
        self.hub.gpa.free(self.frame_buf);
        self.hub.gpa.free(self.reply_buf);
        self.hub.gpa.free(self.scratch);
    }

    pub const CallResult = struct { status: frame.Status, payload: []const u8, correlation: u64 };

    /// Ship one call and bring back the raw reply (the status word and
    /// its payload, which lives in `reply_buf` until the next call).
    pub fn callRaw(self: *Conn, req: verbs.Request) errors.Error!CallResult {
        if (self.poisoned) return error.ConnectionLost;
        const corr = self.next_correlation;
        self.next_correlation += 1;
        var b: codec.Builder = .init(self.frame_buf[frame.HEADER_LEN..]);
        verbs.encodeRequest(req, &b) catch return error.ValueTooLarge;
        const h = self.client.nextHeader(@as(verbs.Verb, req), .ok, corr, false);
        const n = self.client.seal(h, self.frame_buf, b.pos) catch return error.ValueTooLarge;
        if (self.tamper_next) {
            // A hostile peer flips a payload byte AND re-stamps the unkeyed
            // checksum: the MAC is the layer that must refuse it.
            self.tamper_next = false;
            self.frame_buf[frame.HEADER_LEN] ^= 0x01;
            std.mem.writeInt(u64, self.frame_buf[frame.CSUM_OFF..][0..8], frame.frameChecksum(self.frame_buf[0..frame.HEADER_LEN], self.frame_buf[frame.HEADER_LEN..][0..b.pos]), .little);
        }
        // The callee's side.
        const in = self.server.accept(self.frame_buf[0..n]) catch |e| {
            self.poisoned = true;
            stats.cluster_wire_sessions_poisoned.inc();
            return if (e == error.MacFailed) error.MacInvalid else error.ConnectionLost;
        };
        const callee = &self.hub.daemons.items[self.to];
        const served = callee.service.serve(in.header, in.payload, self.peer, self.to, self.reply_buf[frame.HEADER_LEN..][0..frame.CONTROL_CAP], self.scratch);
        const rh = self.server.nextHeader(@as(verbs.Verb, req), served.status, in.header.correlation, true);
        const rn = self.server.seal(rh, self.reply_buf, served.payload.len) catch return error.ValueTooLarge;
        // Back at the caller.
        const rin = self.client.accept(self.reply_buf[0..rn]) catch |e| {
            self.poisoned = true;
            stats.cluster_wire_sessions_poisoned.inc();
            return if (e == error.MacFailed) error.MacInvalid else error.ConnectionLost;
        };
        std.debug.assert(rin.header.isReply() and rin.header.correlation == corr);
        const status = frame.Status.fromWire(rin.header.status) orelse return error.WireSchemaRefused;
        return .{ .status = status, .payload = rin.payload, .correlation = corr };
    }

    /// Ship one call and interpret the reply: the typed reply or the typed
    /// error (the ONE table).
    pub fn call(self: *Conn, req: verbs.Request) errors.Error!verbs.Reply {
        const r = try self.callRaw(req);
        return service.interpretReply(@as(verbs.Verb, req), r.status, r.payload);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// A handler for EVERY verb family: answers each verb with a canned,
/// well-formed reply naming the serving daemon, so a hub test exercises
/// every reply codec through the full frame path.
const FamilyEcho = struct {
    me: u32,
    served: [verbs.Verb.count]u32 = @splat(0),

    fn serve(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *FamilyEcho = @ptrCast(@alignCast(ctx));
        self.served[req.verb.index()] += 1;
        const attrs = std.mem.zeroes(verbs.InodeValueWire);
        return switch (req.payload) {
            .join_appender => .{ .ok = .{ .join_appender = .{ .head = .{ .appender_id = self.me, .already = 0, .n_segments = 0, .n_grant = 0, .reserved = 0, .page_addr = 4096, .node_seq_base = 1 << 38, .ring_bytes = 1 << 19 }, .segments = &.{}, .grant = &.{} } } },
            .extent_grant => .{ .ok = .{ .extent_grant = .{ .head = .{ .n = 0, .reserved = @splat(0) }, .items = &.{} } } },
            .return_extents => .{ .ok = .{ .return_extents = .{ .cleared = 1, .already = 0 } } },
            .leave_appender => .{ .ok = .{ .leave_appender = .fresh } },
            .publish_endpoint => .{ .ok = .{ .publish_endpoint = .fresh } },
            .resolve_endpoint => .{ .ok = .{ .resolve_endpoint = .{ .present = 0, .reserved = @splat(0), .endpoint = .none } } },
            .resolve_set_config => .{ .ok = .{ .resolve_set_config = .{ .head = .{ .generation = 0, .len = 0, .reserved = 0 }, .bytes = &.{} } } },
            .publish_roots => .{ .ok = .{ .publish_roots = .{ .published = 0, .already = 0 } } },
            .grow_ring => .{ .ok = .{ .grow_ring = .{ .present = 0, .reserved = @splat(0), .segment = .{ .start = 0, .len = 0 } } } },
            .acquire_slots, .acquire_slot => |_, tag| if (tag == .acquire_slots) service.Answer{ .ok = .{ .acquire_slots = .{ .head = .{ .kind = 1, .reserved = 0, .n = 0, .already = 0, .slot = 1, .holder = self.me, .g = 2 }, .items = &.{} } } } else service.Answer{ .ok = .{ .acquire_slot = .{ .head = .{ .kind = 1, .reserved = 0, .n = 0, .already = 0, .slot = 1, .holder = self.me, .g = 2 }, .items = &.{} } } },
            .offer_slot => .{ .ok = .{ .offer_slot = .fresh } },
            .release_slot => .{ .ok = .{ .release_slot = .replay } },
            .resolve_slot => |r| .{ .ok = .{ .resolve_slot = .{ .kind = 0, .reserved = @splat(0), .appender_id = self.me, .g = r.slot, .reserved2 = 0 } } },
            .dir_rename_lock => .{ .ok = .{ .dir_rename_lock = .{ .kind = 0, .already = 0, .reserved = 0, .holder = self.me } } },
            .dir_rename_unlock => .{ .ok = .{ .dir_rename_unlock = .fresh } },
            .mark_shared => .{ .ok = .{ .mark_shared = .{ .kind = 0, .already = 0, .reserved = @splat(0) } } },
            .share_block => .{ .ok = .{ .share_block = .{ .inserted = 1, .already = 0 } } },
            .release_shared => .{ .ok = .{ .release_shared = .{ .kind = 1, .reserved = @splat(0), .remaining = 1 } } },
            .block_grant => .{ .ok = .{ .block_grant = .{ .head = .{ .kind = 1, .already = 0, .n = 0, .reserved = 0, .blocks = 0, .population = 0, .frontier = 0 }, .items = &.{} } } },
            .return_blocks => .{ .ok = .{ .return_blocks = .{ .present = 1, .reserved = @splat(0), .cleared = 64 } } },
            .alloc_lease_acquire => .{ .ok = .{ .alloc_lease_acquire = .{ .head = .{ .already = 0, .reserved = 0, .n_bitmap = 0, .reserved2 = 0, .term = 1, .predecessor_blocks = 0 }, .items = &.{} } } },
            .alloc_lease_bitmap => .{ .ok = .{ .alloc_lease_bitmap = .fresh } },
            .alloc_lease_release => .{ .ok = .{ .alloc_lease_release = .replay } },
            .record_recovered => .{ .ok = .{ .record_recovered = .fresh } },
            .record_death => .{ .ok = .{ .record_death = .fresh } },
            .join, .renew => |_, tag| blk: {
                const g: verbs.Grant = .{ .head = .{ .epoch = 1, .term = 1, .granted_at_owner_ms = 0, .ack_prior_epoch = 0, .t_owner_ms = 45_000, .skew_max_ms = 23, .d_purge_ms = 20, .renew_ms = 10_000, .checkpoint_ceiling_ms = 1100, .n_ack = 0, .n_release = 0, .n_offered = 0, .reserved = @splat(0), .config_generation = 0 }, .slot_leases_ack = &.{}, .slot_release_notices = &.{}, .offered_slots = &.{} };
                break :blk if (tag == .join) service.Answer{ .ok = .{ .join = g } } else service.Answer{ .ok = .{ .renew = g } };
            },
            .leave => .{ .ok = .{ .leave = .replay } },
            .census => blk: {
                var b: codec.Builder = .init(scratch);
                var id: [16]u8 = @splat(0);
                @memcpy(id[0..12], &req.peer.?.peerId());
                verbs.CensusPage.encodeRow(&b, .{ .head = .{ .id = id, .boot = @splat(0), .endpoint = .none, .role = 0, .state = 0, .mount_len = 3, .pid = 1, .reserved = 0, .epoch = 1, .age_ms = 0, .expires_in_ms = 1, .acked_free_epoch = 0 }, .mount = "/m0" }) catch unreachable;
                break :blk .{ .ok = .{ .census = .{ .head = .{ .n = 1, .has_next = 0, .reserved = @splat(0), .next = 0 }, .rows_bytes = b.written() } } };
            },
            .token_grant => .{ .ok = .{ .token_grant = .{ .head = .{ .kind = 0, .flags = verbs.PAGE_XATTRS_COMPLETE, .reserved = 0, .n_xattrs = 0, .n_dir = 0, .holder = 0, .attrs = attrs, .reserved2 = @splat(0) }, .xattrs_bytes = "", .dir_bytes = "" } } },
            .token_recall => .{ .ok = .{ .token_recall = .acked } },
            .token_recall_ack => .{ .ok = .{ .token_recall_ack = .fresh } },
            .token_release => |r| .{ .ok = .{ .token_release = .{ .count = @intCast(r.items.len), .reserved = 0 } } },
            .custody_grant => .{ .ok = .{ .custody_grant = .{ .head = .{ .kind = 1, .reserved = @splat(0), .status = 3, .grant_epoch = 0, .page_len = 0, .reserved2 = 0 }, .page_bytes = "" } } },
            .xv_step => .{ .ok = .{ .xv_step = .{ .status = 0, .has_inode = 0, .reserved = @splat(0), .inode = attrs, .reserved2 = @splat(0) } } },
            .lookup_exact => .{ .ok = .{ .lookup_exact = .{ .present = 0, .file_type = 0, .reserved = @splat(0), .ino = 0 } } },
            .xv_guards => .{ .ok = .{ .xv_guards = .fresh } },
            .xv_release => .{ .ok = .{ .xv_release = .fresh } },
            .record_ship => .{ .ok = .{ .record_ship = .{ .kind = 0, .has_inode = 0, .reserved = 0, .holder = 0, .inode = attrs, .reserved2 = @splat(0) } } },
            .free_blocks => |r| blk: {
                const n = r.items.len;
                @memset(scratch[0..n], 1);
                break :blk .{ .ok = .{ .free_blocks = .{ .head = .{ .n = @intCast(n), .reserved = @splat(0) }, .verdicts = scratch[0..n] } } };
            },
            .supply_stripe_inos => .{ .ok = .{ .supply_stripe_inos = .{ .head = .{ .n = 0, .reserved = @splat(0) }, .items = &.{} } } },
            // @anchor:ops_jobs_loopback_echo — the job block's canned shapes.
            .job_enroll => .{ .ok = .{ .job_enroll = .{ .heartbeat_ms = 10_000, .lease_ttl_ms = 30_000 } } },
            .shard_assign => .{ .ok = .{ .shard_assign = .none } },
            .heartbeat => .{ .ok = .{ .heartbeat = .{ .lease_ttl_ms = 30_000, .verdict = 0, .reserved = @splat(0) } } },
            .result_submit => .{ .ok = .{ .result_submit = .of(.accepted) } },
            .read_shard_result => .{ .ok = .{ .read_shard_result = .fresh } },
            .shard_abandon => .{ .ok = .{ .shard_abandon = .fresh } },
            // @anchor:ops_admin_loopback_echo — the admin lane's canned shapes.
            .admin_job_list => .{ .ok = .{ .admin_job_list = .{ .head = .{ .n = 0, .verdict = 0, .reserved = @splat(0) }, .rows = &.{} } } },
            .admin_job_control => .{ .ok = .{ .admin_job_control = .of(.ok) } },
            .admin_job_submit => .{ .ok = .{ .admin_job_submit = .{ .job_id = 1, .verdict = 0, .reserved = @splat(0) } } },
            .admin_fsck_report => .{ .ok = .{ .admin_fsck_report = .{ .head = .{ .len = 0, .verdict = 0, .reserved = @splat(0) }, .json = "" } } },
            .admin_volume => .{ .ok = .{ .admin_volume = .refusal(.ok) } },
            .admin_rekey => .{ .ok = .{ .admin_rekey = .refusal(.ok, .none) } },
            .admin_fsck => .{ .ok = .{ .admin_fsck = .{ .job_id = 1, .verdict = 0, .reserved = @splat(0) } } },
            .ping, .challenge, .prove, .admitted, .refused => unreachable,
        };
    }

    fn registerAll(self: *FamilyEcho, svc: *service.Service) void {
        inline for (@typeInfo(verbs.Verb).@"enum".field_values) |value| {
            const v: verbs.Verb = @fromBackingInt(@intCast(value));
            if (!v.isHandshake() and v != .ping) svc.register(v, .{ .ctx = self, .serve = serve });
        }
    }
};

fn identityOf(n: u64) verbs.WireIdentity {
    return .{ .node_token = 0x1000 + n, .writer_id = 0x2000 + n, .mount_slot = @intCast(n), .reserved = 0 };
}

test "four in-process daemons exchange every verb family's shape over the hub: each call's reply decodes at the caller and names the callee; the identity-bearing peer travels" {
    const gpa = testing.allocator;
    var hub = LoopbackHub.init(gpa, testing.io);
    defer hub.deinit();
    const secret: auth.Secret = @splat(0x77);
    var services: [4]service.Service = @splat(.init);
    var echoes: [4]FamilyEcho = undefined;
    for (0..4) |i| {
        echoes[i] = .{ .me = @intCast(100 + i) };
        echoes[i].registerAll(&services[i]);
        _ = try hub.addDaemon(&services[i], secret, identityOf(i));
    }
    // Every ordered pair dials.
    var conns: [4][4]?Conn = @splat(@splat(null));
    defer for (&conns) |*row| for (row) |*c| if (c.*) |*cc| cc.deinit();
    for (0..4) |from| for (0..4) |to| if (from != to) {
        conns[from][to] = try hub.connect(from, to);
        try testing.expect(conns[from][to].?.peer.?.sameMember(identityOf(from)));
    };
    const me = identityOf(0);
    var c = &conns[0][1].?;
    // Manager region + slots.
    const j = try c.call(.{ .join_appender = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .identity = me, .ring_want_bytes = 0 } });
    try testing.expectEqual(@as(u32, 101), j.join_appender.head.appender_id);
    const rs = try c.call(.{ .resolve_slot = .{ .volume = 0, .slot = 17, .reserved = 0 } });
    try testing.expectEqual(@as(u32, 17), rs.resolve_slot.g);
    _ = try c.call(.{ .extent_grant = .{ .volume = 0, .reserved = 0, .appender_id = 100, .want = 0, .reserved2 = 0 } });
    _ = try c.call(.{ .return_extents = .{ .head = .{ .volume = 0, .n_runs = 0, .appender_id = 100, .reserved = 0 }, .items = &.{} } });
    _ = try c.call(.{ .acquire_slots = .{ .volume = 0, .want = 4, .appender_id = 100 } });
    const asl = try c.call(.{ .acquire_slot = .{ .volume = 0, .slot = 1, .appender_id = 100 } });
    try testing.expectEqual(@as(u8, 1), asl.acquire_slot.head.kind);
    _ = try c.call(.{ .offer_slot = .{ .volume = 0, .slot = 1, .appender_id = 100, .to = 102, .reserved = 0 } });
    const words: verbs.SlotWordsWire = .{ .root = .{ .addr = 0, .seq = 0 }, .cursor = 2, .seq_floor = 1, .slot_tree_extents = 0, .reserved = 0 };
    const rel = try c.call(.{ .release_slot = .{ .head = .{ .volume = 0, .slot = 1, .appender_id = 100, .g = 2, .n_tails = 0, .words = words }, .items = &.{} } });
    try testing.expectEqual(@as(u8, 1), rel.release_slot.already);
    _ = try c.call(.{ .leave_appender = .{ .head = .{ .volume = 0, .n_unclaimed = 0, .appender_id = 100, .identity = me, .reserved = 0 }, .items = &.{} } });
    _ = try c.call(.{ .publish_endpoint = .{ .volume = 0, .reserved = 0, .appender_id = 100, .identity = me, .pr_key = 0, .endpoint = .none, .reserved2 = @splat(0) } });
    _ = try c.call(.{ .resolve_endpoint = .{ .volume = 0, .reserved = 0, .appender_id = 101 } });
    _ = try c.call(.{ .resolve_set_config = .{ .volume = 0, .reserved = @splat(0) } });
    _ = try c.call(.{ .publish_roots = .{ .head = .{ .volume = 0, .reserved = 0, .appender_id = 100, .n = 0, .reserved2 = 0 }, .items = &.{} } });
    _ = try c.call(.{ .grow_ring = .{ .volume = 0, .reserved = 0, .appender_id = 100, .want_bytes = 1 << 20 } });
    // Rename, clone, alloc, death.
    const dl = try c.call(.{ .dir_rename_lock = .{ .volume = 0, .reserved = 0, .appender_id = 100 } });
    try testing.expectEqual(@as(u32, 101), dl.dir_rename_lock.holder);
    _ = try c.call(.{ .dir_rename_unlock = .{ .volume = 0, .reserved = 0, .appender_id = 100 } });
    _ = try c.call(.{ .mark_shared = .{ .volume = 0, .reserved = 0, .block_index = 0, .vol_tag = 11, .block_idx = 5, .owner_ino = 9 } });
    _ = try c.call(.{ .share_block = .{ .head = .{ .volume = 0, .n = 0, .reserved = 0, .vol_tag = 11, .block_idx = 5 }, .items = &.{} } });
    _ = try c.call(.{ .release_shared = .{ .volume = 0, .present = 0, .reserved = 0, .reserved2 = 0, .vol_tag = 11, .block_idx = 5, .owner = .{ .owner_ino = 0, .block_index = 0, .reserved = 0 } } });
    _ = try c.call(.{ .block_grant = .{ .volume = 0, .reserved = 0, .want = 0, .writer = me, .vol_tag = 11, .held_unconsumed = 0 } });
    _ = try c.call(.{ .return_blocks = .{ .volume = 0, .reserved = 0, .len = 64, .writer = me, .vol_tag = 11, .start = 0 } });
    _ = try c.call(.{ .alloc_lease_acquire = .{ .volume = 0, .home_vol = 0, .appender_id = 100, .identity = me, .vol_tag = 11, .blocks = 1000 } });
    _ = try c.call(.{ .alloc_lease_bitmap = .{ .head = .{ .volume = 0, .n = 0, .reserved = 0, .identity = me, .vol_tag = 11, .term = 1 }, .items = &.{} } });
    _ = try c.call(.{ .alloc_lease_release = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .identity = me, .vol_tag = 11, .term = 1 } });
    _ = try c.call(.{ .record_death = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .member = identityOf(3), .epoch = 1, .pr_key = 0 } });
    _ = try c.call(.{ .record_recovered = .{ .volume = 0, .vol = 0, .reserved = 0, .member = identityOf(3) } });
    // Membership.
    var id: [16]u8 = @splat(0);
    @memcpy(id[0..12], &me.peerId());
    const g = try c.call(.{ .join = .{ .head = .{ .role = 0, .flags = 0, .mount_len = 3, .pid = 1, .id = id, .boot = @splat(0), .endpoint = .none, .reserved = @splat(0), .prior_epoch = 0, .pr_key = 0 }, .bytes = "/m0" } });
    try testing.expectEqual(@as(u32, 45_000), g.join.head.t_owner_ms);
    _ = try c.call(.{ .renew = .{ .head = .{ .id = id, .epoch = 1, .acked_free_epoch = 0, .n_windows = 0, .reserved = @splat(0) }, .windows_bytes = "" } });
    _ = try c.call(.{ .leave = .{ .id = id } });
    const census = try c.call(.{ .census = .{ .cursor = 0, .limit = 10, .reserved = 0 } });
    var rows = census.census.rows();
    const row = rows.next().?;
    try testing.expectEqualSlices(u8, id[0..12], row.head.id[0..12]);
    try testing.expectEqualStrings("/m0", row.mount);
    // Tokens.
    const tg = try c.call(.{ .token_grant = .{ .head = .{ .volume = 0, .wants = verbs.WANT_RECORDS, .mode = 0, .xattr_after_len = 0, .client_id = 1, .object = 9, .after = 0 }, .bytes = "" } });
    try testing.expectEqual(@as(u8, 0), tg.token_grant.head.kind);
    const recalled = [_]u64{9};
    const rv = try c.call(.{ .token_recall = .{ .head = .{ .volume = 0, .reserved = 0, .n = 1, .frame_id = 1, .holder = 1 }, .items = &recalled } });
    try testing.expectEqual(@as(u8, 0), rv.token_recall.kind);
    _ = try c.call(.{ .token_recall_ack = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .client_id = 1, .frame_id = 0 } });
    const objs = [_]u64{ 9, 10 };
    const tr = try c.call(.{ .token_release = .{ .head = .{ .volume = 0, .n = 2, .reserved = 0, .client_id = 1 }, .items = &objs } });
    try testing.expectEqual(@as(u32, 2), tr.token_release.count);
    const cg = try c.call(.{ .custody_grant = .{ .volume = 0, .concurrent_write = 0, .reserved = 0, .wait_ms = 0, .client_id = 1, .object = 9, .span_start = 0, .span_len = 1, .lease_epoch = 0, .reserved2 = 0 } });
    try testing.expectEqual(@as(u8, 1), cg.custody_grant.head.kind);
    // Cross-owner + record ship + frees.
    const step: verbs.XvStepWire = .{ .kind = 4, .parent_update = 0, .name_len = 0, .flags = 0, .ft_bits = 0, .a = 9, .b = 0, .c = 5, .w0 = 0, .w1 = 0, .w2 = 0, .w3 = 0 };
    _ = try c.call(.{ .xv_step = .{ .head = .{ .volume = 0, .step_idx = 0, .reserved = 0, .tx_id = 1, .scope = 0, .step = step }, .name = "" } });
    _ = try c.call(.{ .lookup_exact = .{ .head = .{ .volume = 0, .name_len = 1, .reserved = 0, .reserved2 = 0, .parent = 1 }, .bytes = "a" } });
    _ = try c.call(.{ .xv_guards = .{ .head = .{ .volume = 0, .n_inodes = 0, .n_dentries = 0, .reserved = 0, .scope = 5 }, .inodes = &.{}, .dentries_bytes = "" } });
    _ = try c.call(.{ .xv_release = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .scope = 5, .ino = 9 } });
    _ = try c.call(.{ .record_ship = .{ .head = .{ .volume = 0, .kind = 3, .reserved = 0, .name_len = 1, .n_proofs = 0, .value_len = 0, .reserved3 = 0, .ino = 9, .lease_epoch = 0, .setattr = .none }, .name = "a", .value = "" } });
    const fv = try c.call(.{ .free_blocks = .{ .head = .{ .volume = 0, .n = 2, .reserved = 0, .vol_tag = 11, .lease_epoch = 0 }, .items = &objs } });
    try testing.expectEqual(@as(usize, 2), fv.free_blocks.verdicts.len);
    const ss = try c.call(.{ .supply_stripe_inos = .{ .head = .{ .volume = 0, .n = 2, .reserved = 0, .dir = 9, .mode = 0o755, .uid = 0, .gid = 0, .reserved2 = 0, .ts_ns = 1 }, .indices = &.{ 0, 1 } } });
    try testing.expectEqual(@as(usize, 0), ss.supply_stripe_inos.items.len);
    // Ping is the service's own.
    // @anchor:ops_jobs_loopback_calls — the job block's six verbs.
    _ = try c.call(.{ .job_enroll = .{ .pr_key = 0, .caps = 0, .reserved = 0 } });
    const sa = try c.call(.{ .shard_assign = .{ .caps = 0, .reserved = 0 } });
    try testing.expect(sa.shard_assign.isNone());
    _ = try c.call(.{ .heartbeat = .{ .job_id = 1, .shard = 1, .reserved = 0, .shard_fencing = 0 } });
    _ = try c.call(.{ .result_submit = .{ .head = .{ .ref = .{ .job_id = 1, .shard = 1, .reserved = 0, .shard_fencing = 0 }, .n = 0, .reserved = 0 }, .checksums = &.{} } });
    _ = try c.call(.{ .read_shard_result = .{ .head = .{ .ref = .{ .job_id = 1, .shard = 1, .reserved = 0, .shard_fencing = 0 }, .len = 0, .reserved = 0 }, .payload = "" } });
    _ = try c.call(.{ .shard_abandon = .{ .ref = .{ .job_id = 1, .shard = 1, .reserved = 0, .shard_fencing = 0 }, .reason = 1, .reserved = @splat(0) } });
    // @anchor:ops_admin_loopback_calls — the admin lane's five verbs.
    const al = try c.call(.{ .admin_job_list = .{ .reserved = 0 } });
    try testing.expectEqual(@as(usize, 0), al.admin_job_list.rows.len);
    _ = try c.call(.{ .admin_job_control = .{ .job_id = 1, .op = 0, .pct = 0, .reserved = @splat(0) } });
    _ = try c.call(.{ .admin_job_submit = .{ .job_type = 1, .throttle_pct = 0, .paused = 0, .reserved = @splat(0), .arg0 = 1, .arg1 = 0 } });
    _ = try c.call(.{ .admin_fsck_report = .{ .job_id = 1 } });
    _ = try c.call(.{ .admin_volume = .{ .head = .{ .op = 2, .no_rebalance = 0, .reserved = @splat(0), .vol_tag = 7, .uri_len = 0, .reserved2 = 0 }, .uri = "" } });
    _ = try c.call(.{ .admin_rekey = .{ .kdf_salt = @splat(1), .key_id = @splat(2), .throttle_pct = 0, .reserved = @splat(0) } });
    _ = try c.call(.{ .admin_fsck = .{ .flags = 0, .throttle_pct = 0, .reserved = @splat(0), .shard_k = 0, .shard_n = 0 } });
    _ = try c.call(.{ .ping = .{} });
    // Every verb reached daemon 1 exactly once (ping is served before the table).
    var total: u32 = 0;
    for (echoes[1].served) |n| total += n;
    try testing.expectEqual(@as(u32, verbs.Verb.count - 5), total);
    // The other pairs serve too, each naming itself.
    for (0..4) |from| for (0..4) |to| if (from != to) {
        const r = try conns[from][to].?.call(.{ .resolve_slot = .{ .volume = 0, .slot = 1, .reserved = 0 } });
        try testing.expectEqual(@as(u32, @intCast(100 + to)), r.resolve_slot.appender_id);
    };
}

test "a stranger without the secret is refused at the handshake; a byte tampered in flight is refused before the service sees it and poisons the connection; the typed statuses travel" {
    const gpa = testing.allocator;
    var hub = LoopbackHub.init(gpa, testing.io);
    defer hub.deinit();
    const secret: auth.Secret = @splat(0x11);
    const wrong: auth.Secret = @splat(0x22);
    var svc: service.Service = .init;
    var echo: FamilyEcho = .{ .me = 1 };
    echo.registerAll(&svc);
    var stranger_svc: service.Service = .init;
    _ = try hub.addDaemon(&svc, secret, identityOf(0));
    _ = try hub.addDaemon(&stranger_svc, wrong, identityOf(1));
    _ = try hub.addDaemon(&svc, secret, identityOf(2));
    const refused_before = stats.cluster_wire_admissions_refused.load();
    try testing.expectError(error.HandshakeRefused, hub.connect(1, 0));
    try testing.expectEqual(refused_before + 1, stats.cluster_wire_admissions_refused.load());
    var c = try hub.connect(2, 0);
    defer c.deinit();
    const served_before = echo.served[verbs.Verb.resolve_slot.index()];
    c.tamper_next = true;
    stats.cluster_wire_mac_failures.resetLogBudgetForTest();
    @import("../core/log.zig").testing.expectErrorLogs(1);
    try testing.expectError(error.MacInvalid, c.call(.{ .resolve_slot = .{ .volume = 0, .slot = 1, .reserved = 0 } }));
    try @import("../core/log.zig").testing.expectAllFired();
    try testing.expectEqual(served_before, echo.served[verbs.Verb.resolve_slot.index()]);
    try testing.expect(c.poisoned);
    try testing.expectError(error.ConnectionLost, c.call(.{ .ping = .{} }));
    // A fresh connection: an unregistered block answers its "not armed" word as a typed error.
    var c2 = try hub.connect(0, 2);
    defer c2.deinit();
    svc.unregister(.token_grant);
    try testing.expectError(error.StaleHolderView, c2.call(.{ .token_grant = .{ .head = .{ .volume = 0, .wants = 0, .mode = 0, .xattr_after_len = 0, .client_id = 1, .object = 9, .after = 0 }, .bytes = "" } }));
    try testing.expectEqual(errors.Class.retryable, errors.classOf(error.StaleHolderView));
}
