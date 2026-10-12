//! The cluster wire end to end over real TCP on loopback (the Linux
//! venue): the listener's threads under their contract names, two dialers'
//! muxes pipelining 10 k verbs with the in-flight bound, the connection
//! cap, a stranger refused, a severed connection surfacing
//! `HolderUnreachable` (EAGAIN) with the client re-dialing, and the
//! bounded dial retry.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const thread_name = @import("../core/thread_name.zig");
const futex = @import("../sync/futex.zig");
const verbs = @import("verbs.zig");
const auth = @import("auth.zig");
const service = @import("service.zig");
const session = @import("session.zig");
const endpoint = @import("endpoint.zig");
const listener = @import("listener.zig");
const dialer = @import("dialer.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("wire/tcp_e2e_linux is Linux only; gate the import on builtin.os.tag");
}

const testing = std.testing;

const Echo = struct {
    served: std.atomic.Value(u64) = .init(0),

    fn serve(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *Echo = @ptrCast(@alignCast(ctx));
        _ = self.served.fetchAdd(1, .monotonic);
        switch (req.payload) {
            .resolve_slot => |r| {
                const peer_slot: u32 = if (req.peer) |p| p.mount_slot else 0xFFFF_FFFF;
                return .{ .ok = .{ .resolve_slot = .{ .kind = 0, .reserved = @splat(0), .appender_id = peer_slot, .g = r.slot, .reserved2 = 0 } } };
            },
            else => return .{ .status = .{ .status = .not_manager } },
        }
    }
};

const secret: auth.Secret = @splat(0x5C);

fn identityOf(n: u32) verbs.WireIdentity {
    return .{ .node_token = 0xABC0 + n, .writer_id = 1, .mount_slot = n, .reserved = 0 };
}

fn startListener(svc: *service.Service) !*listener.Listener {
    return listener.Listener.start(testing.allocator, testing.io, .{ .explicit = try endpoint.parseEndpoint("127.0.0.1:0") }, svc, secret, .{ .raw_cpus = 4, .nofile = 1 << 16 });
}

/// `/proc/self/task/*/comm` holds `name` on some live thread.
fn threadNamed(name: []const u8) !bool {
    const io = testing.io;
    var dir = try std.Io.Dir.cwd().openDir(io, "/proc/self/task", .{ .iterate = true });
    defer dir.close(io);
    var it = dir.iterate();
    var path_buf: [64]u8 = undefined;
    while (try it.next(io)) |e| {
        const p = try std.fmt.bufPrint(&path_buf, "{s}/comm", .{e.name});
        var buf: [32]u8 = undefined;
        const n = dir.readFile(io, p, &buf) catch continue;
        const comm = std.mem.trimEnd(u8, n, "\n");
        if (std.mem.eql(u8, comm, name)) return true;
    }
    return false;
}

const Storm = struct {
    client: *dialer.Client,
    calls: u32,
    slot_base: u16,
    ok: u32 = 0,
    failed: ?anyerror = null,

    fn run(self: *Storm) void {
        var out: [256]u8 = undefined;
        var i: u32 = 0;
        while (i < self.calls) : (i += 1) {
            const slot: u16 = self.slot_base +% @as(u16, @intCast(i % 1000));
            const rep = self.client.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = slot, .reserved = 0 } }, &out) catch |e| {
                self.failed = e;
                return;
            };
            if (rep.resolve_slot.g != slot) {
                self.failed = error.WrongReply;
                return;
            }
            self.ok += 1;
        }
    }
};

/// Several callers share ONE client (one mux, K in flight on ONE connection).
const Pipeline = struct {
    client: *dialer.Client,
    threads: usize,
    calls_per_thread: u32,

    fn run(self: Pipeline) !u32 {
        var storms: [8]Storm = undefined;
        var handles: [8]std.Thread = undefined;
        for (0..self.threads) |t| {
            storms[t] = .{ .client = self.client, .calls = self.calls_per_thread, .slot_base = @intCast(t * 1000) };
            handles[t] = try std.Thread.spawn(.{}, Storm.run, .{&storms[t]});
        }
        var total: u32 = 0;
        for (0..self.threads) |t| {
            handles[t].join();
            if (storms[t].failed) |e| return e;
            total += storms[t].ok;
        }
        return total;
    }
};

test "tcp e2e: a listener and two dialers run 10k verbs pipelined on two connections under the in-flight bound; the threads carry their contract names; the peer identity travels" {
    var svc: service.Service = .init;
    var echo: Echo = .{};
    svc.register(.resolve_slot, .{ .ctx = &echo, .serve = Echo.serve });
    const l = try startListener(&svc);
    defer l.stop();
    try testing.expect(l.advertised.port != 0);
    try testing.expect(try threadNamed(listener.ACCEPT_THREAD_NAME));

    var c1 = dialer.Client.init(testing.allocator, testing.io, l.advertised, secret, identityOf(1));
    defer c1.deinit();
    var c2 = dialer.Client.init(testing.allocator, testing.io, l.advertised, secret, identityOf(2));
    defer c2.deinit();
    // The first call dials; the served reply names the admitted peer.
    var out: [256]u8 = undefined;
    const r1 = try c1.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 1, .reserved = 0 } }, &out);
    try testing.expectEqual(@as(u32, 1), r1.resolve_slot.appender_id);
    const r2 = try c2.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 2, .reserved = 0 } }, &out);
    try testing.expectEqual(@as(u32, 2), r2.resolve_slot.appender_id);
    try testing.expectEqual(@as(u32, 2), l.liveConnections());
    try testing.expect(try threadNamed(listener.CONN_THREAD_NAME));
    try testing.expect(try threadNamed(dialer.MUX_THREAD_NAME));
    // 10,000 verbs: 5,000 per client, 4 caller threads each (≤ 64 in flight per session).
    const before = echo.served.load(.monotonic);
    const sent_before = stats.cluster_wire_frames_sent.load();
    var pipe1_result: anyerror!u32 = 0;
    const t1 = try std.Thread.spawn(.{}, struct {
        fn run(p: Pipeline, res: *anyerror!u32) void {
            res.* = p.run();
        }
    }.run, .{ Pipeline{ .client = &c1, .threads = 4, .calls_per_thread = 1250 }, &pipe1_result });
    const total2 = try (Pipeline{ .client = &c2, .threads = 4, .calls_per_thread = 1250 }).run();
    t1.join();
    const total1 = try pipe1_result;
    try testing.expectEqual(@as(u32, 5000), total1);
    try testing.expectEqual(@as(u32, 5000), total2);
    try testing.expectEqual(before + 10_000, echo.served.load(.monotonic));
    try testing.expect(stats.cluster_wire_frames_sent.load() - sent_before >= 20_000);
    // Still exactly two connections: K in flight costs ONE connection.
    try testing.expectEqual(@as(u32, 2), l.liveConnections());
    try testing.expect(!c1.mux.?.deadOnArrival());
}

test "tcp e2e: a severed connection surfaces HolderUnreachable (EAGAIN) to the caller whose reply was lost, poisons the session, and the client re-dials on its next call" {
    var svc: service.Service = .init;
    var echo: Echo = .{};
    svc.register(.resolve_slot, .{ .ctx = &echo, .serve = Echo.serve });
    const l = try startListener(&svc);
    defer l.stop();
    var c = dialer.Client.init(testing.allocator, testing.io, l.advertised, secret, identityOf(7));
    defer c.deinit();
    var out: [256]u8 = undefined;
    _ = try c.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 1, .reserved = 0 } }, &out);
    const mux = c.mux.?;
    const poisoned_before = stats.cluster_wire_sessions_poisoned.load();
    const reconnects_before = stats.cluster_wire_reconnects.load();
    try testing.expectEqual(@as(u32, 1), l.severAllConnections());
    // The reader sees EOF and poisons; a call racing it is HolderUnreachable
    // or ConnectionLost — both EAGAIN/EIO-class typed, never a hang.
    var i: u32 = 0;
    while (!mux.isPoisoned() and i < 10_000) : (i += 1) testing.io.sleep(.fromMilliseconds(1), .awake) catch {};
    try testing.expect(mux.isPoisoned());
    try testing.expectEqual(poisoned_before + 1, stats.cluster_wire_sessions_poisoned.load());
    try testing.expect(mux.deadOnArrival());
    // The client re-dials: the next call lands on a fresh session.
    const r = try c.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 9, .reserved = 0 } }, &out);
    try testing.expectEqual(@as(u32, 9), r.resolve_slot.g);
    try testing.expectEqual(reconnects_before + 1, stats.cluster_wire_reconnects.load());
    try testing.expect(c.mux.? != mux);
    try testing.expectEqual(linux.E.AGAIN, errors.toErrno(error.HolderUnreachable));
    // A call PARKED at the holder when the session dies: the caller gets
    // the typed word, the holder's handler is never interrupted.
    var hold: Hold = .{};
    svc.register(.resolve_slot, .{ .ctx = &hold, .serve = Hold.serve });
    var parked_result: ?anyerror = null;
    const t = try std.Thread.spawn(.{}, Parked.run, .{ c.mux.?, &parked_result });
    i = 0;
    while (hold.entered.load(.acquire) == 0 and i < 10_000) : (i += 1) testing.io.sleep(.fromMilliseconds(1), .awake) catch {};
    try testing.expectEqual(@as(u32, 1), hold.entered.load(.acquire));
    try testing.expectEqual(@as(u32, 1), l.severAllConnections());
    t.join();
    try testing.expectEqual(@as(?anyerror, error.HolderUnreachable), parked_result);
    hold.gate.store(1, .release);
    futex.wakeAll(&hold.gate);
}

/// A handler that parks until released (the slow holder).
const Hold = struct {
    entered: std.atomic.Value(u32) = .init(0),
    gate: futex.Word = .init(0),

    fn serve(ctx: *anyopaque, _: *const service.Request, _: []u8) service.Answer {
        const self: *Hold = @ptrCast(@alignCast(ctx));
        _ = self.entered.fetchAdd(1, .acq_rel);
        while (self.gate.load(.acquire) == 0) futex.wait(&self.gate, 0);
        return .{ .ok = .{ .resolve_slot = .{ .kind = 1, .reserved = @splat(0), .appender_id = 0, .g = 0, .reserved2 = 0 } } };
    }
};

const Parked = struct {
    fn run(m: *dialer.Mux, res: *?anyerror) void {
        var o: [64]u8 = undefined;
        _ = m.call(.{ .resolve_slot = .{ .volume = 0, .slot = 3, .reserved = 0 } }, &o) catch |e| {
            res.* = e;
            return;
        };
        res.* = null;
    }
};

test "tcp e2e: a stranger without the set's secret is refused at the handshake (HandshakeRefused, EACCES); the listener's cap refuses the over-cap peer before its challenge and the bounded retry ends in ListenerRefused" {
    var svc: service.Service = .init;
    const l = try listener.Listener.start(testing.allocator, testing.io, .{ .explicit = try endpoint.parseEndpoint("127.0.0.1:0") }, &svc, secret, .{ .raw_cpus = 1, .nofile = 1 << 16 });
    defer l.stop();
    try testing.expectEqual(listener.MAX_CONNS_FLOOR, l.max_conns);
    const wrong: auth.Secret = @splat(0x00);
    const peer_id = identityOf(3).peerId();
    const refused_before = stats.cluster_wire_admissions_refused.load();
    log.testing.expectErrorLogs(0);
    try testing.expectError(error.HandshakeRefused, dialer.connect(testing.allocator, testing.io, l.advertised, &wrong, &peer_id));
    try testing.expectEqual(refused_before + 1, stats.cluster_wire_admissions_refused.load());
    try testing.expectEqual(linux.E.ACCES, errors.toErrno(error.HandshakeRefused));
    // Fill the cap with raw connections that never prove (each costs the
    // listener one parked handshake), then the 65th is accepted-and-closed.
    var raw: [listener.MAX_CONNS_FLOOR]std.Io.net.Stream = undefined;
    var opened: usize = 0;
    defer for (raw[0..opened]) |s| s.close(testing.io);
    const addr = l.advertised.toIp().?;
    while (opened < raw.len) : (opened += 1) {
        raw[opened] = try std.Io.net.IpAddress.connect(&addr, testing.io, .{ .mode = .stream });
    }
    var i: u32 = 0;
    while (l.liveConnections() < listener.MAX_CONNS_FLOOR and i < 10_000) : (i += 1) testing.io.sleep(.fromMilliseconds(1), .awake) catch {};
    try testing.expectEqual(listener.MAX_CONNS_FLOOR, l.liveConnections());
    const refused_conns_before = stats.cluster_wire_connections_refused.load();
    try testing.expectError(error.BeforeChallenge, dialer.connectOnce(testing.allocator, testing.io, l.advertised, &secret, &peer_id, .plaintext));
    try testing.expectEqual(refused_conns_before + 1, stats.cluster_wire_connections_refused.load());
    try testing.expectEqual(linux.E.AGAIN, errors.toErrno(error.ListenerRefused));
    try testing.expectEqual(linux.E.AGAIN, errors.toErrno(error.QueueFull));
    // Free one slot: the dial lands.
    raw[0].close(testing.io);
    opened -= 1;
    raw[0] = raw[opened];
    i = 0;
    while (l.liveConnections() >= listener.MAX_CONNS_FLOOR and i < 10_000) : (i += 1) testing.io.sleep(.fromMilliseconds(1), .awake) catch {};
    const retries_before = stats.cluster_wire_dial_retries.load();
    const m = try dialer.connect(testing.allocator, testing.io, l.advertised, &secret, &peer_id);
    defer m.close();
    try testing.expectEqual(retries_before, stats.cluster_wire_dial_retries.load());
    _ = thread_name.classify(listener.CONN_THREAD_NAME);
}
