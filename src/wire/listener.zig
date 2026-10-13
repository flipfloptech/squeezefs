//! The ONE listener of a daemon (`docs/porting/sym_wire_membership_
//! recovery.md` §1.4; decision 0002 row 227: one socket, one cap, one
//! endpoint for every verb family — the reference's second membership
//! listener is gone): an accept thread `sqz-wire-acc` on `accept(2)`, a
//! connection cap claimed BEFORE anything is spawned, and one
//! `sqz-wire-conn` OS thread per admitted connection running the ladder
//! and then the request loop (receive → service → reply). The connection
//! threads are OS threads BY DESIGN (charter §9 "thread-per-connection
//! for wires"): the wire is not on the io_uring hot path, so a blocking
//! socket read parks one thread and nothing else.
//!
//! The cap: `clamp(raw_cpus × 16, 64, nofile / 8)` — the root is the RAW
//! affinity mask, never the fleet-share-divided one: a listener's load is
//! the FLEET's width × each member's session demand, which N co-located
//! daemons each serve WHOLE ("the divisor shrank the cap exactly as the
//! fleet grew — 64 on a 32-member fleet on 32 cores, the 14th member
//! refused at accept"). `SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS` wins verbatim,
//! railed by the fd ceiling only, the rail announced.
//!
//! Linux only: the thread names, the raw socket syscalls for the default
//! route lookup, and the futex the stop waits on are the Linux venue's.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const net = Io.net;
const linux = std.os.linux;
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const thread_name = @import("../core/thread_name.zig");
const derive = @import("../core/derive.zig");
const futex = @import("../sync/futex.zig");
const frame = @import("frame.zig");
const codec = @import("codec.zig");
const verbs = @import("verbs.zig");
const auth = @import("auth.zig");
const session = @import("session.zig");
const service = @import("service.zig");
const endpoint = @import("endpoint.zig");
const stats = @import("stats.zig");
const tls = @import("tls.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("wire/listener is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const ACCEPT_THREAD_NAME = "sqz-wire-acc";
pub const CONN_THREAD_NAME = "sqz-wire-conn";

comptime {
    std.debug.assert(thread_name.fitsBudget(ACCEPT_THREAD_NAME));
    std.debug.assert(thread_name.fitsBudget(CONN_THREAD_NAME));
}

/// "The shipped factor kept — nothing has measured the thread-per-
/// connection wake storm at another factor."
pub const CONNS_PER_CORE: u32 = 16;
/// The shipped posture; never regress below it.
pub const MAX_CONNS_FLOOR: u32 = 64;
/// A connection costs its socket and the dialer's shutdown-nudge dup.
pub const CONNECTION_FDS: u32 = 2;
/// The listener may spend a quarter of `RLIMIT_NOFILE` (the `fd_cache_cap` law).
pub const LISTENER_FD_SHARE: u32 = 4;
/// A serve that holds a session this long is named loud: the request loop
/// is sequential per connection, so a parked serve (a grant on an object
/// in flight, a guard behind a held lock) stalls every call the dialer
/// multiplexed on the session — a tenth of the dialer's reply bound, so
/// the warning lands before the callers time out.
pub const SLOW_SERVE_WARN_MS: u64 = session.DIAL_TIMEOUT_MS / 10;
/// Accept errors ride a doubling backoff: a bare `continue` turned EMFILE
/// into a busy loop.
pub const ACCEPT_BACKOFF_MIN_MS: u64 = 5;
pub const ACCEPT_BACKOFF_MAX_MS: u64 = 1000;

/// The fd ceiling's share of connections.
pub fn fdCeiling(nofile: u64) u32 {
    const c = nofile / (CONNECTION_FDS * LISTENER_FD_SHARE);
    return @intCast(@min(c, std.math.maxInt(u32)));
}

/// `clamp(raw_cpus × 16, 64, nofile / 8)` (floor wins a tie, like every
/// derivation here: a tiny nofile is the operator's to raise).
pub fn deriveMaxConnections(raw_cpus: u32, nofile: u64) u32 {
    return @intCast(derive.clamp(@as(u64, raw_cpus) * CONNS_PER_CORE, MAX_CONNS_FLOOR, fdCeiling(nofile)));
}

pub const Resolved = struct { value: u32, railed: bool };

/// `explicit > derived`: an explicit cap wins verbatim, railed by the fd
/// ceiling only (and never below the floor).
pub fn resolveMaxConnections(explicit: ?u32, raw_cpus: u32, nofile: u64) Resolved {
    if (explicit) |e| {
        const ceiling = @max(fdCeiling(nofile), MAX_CONNS_FLOOR);
        if (e > ceiling) return .{ .value = ceiling, .railed = true };
        return .{ .value = e, .railed = false };
    }
    return .{ .value = deriveMaxConnections(raw_cpus, nofile), .railed = false };
}

/// The soft `RLIMIT_NOFILE` raised to the hard limit once per process,
/// the as-found value recorded first (what the cap is judged against).
pub fn raiseNofile() u64 {
    const lim = std.posix.getrlimit(.NOFILE) catch return 1024;
    if (lim.cur < lim.max) {
        std.posix.setrlimit(.NOFILE, .{ .cur = lim.max, .max = lim.max }) catch return lim.cur;
        return lim.max;
    }
    return lim.cur;
}

/// The ONE bind knob: absent = `auto`; `off` is a posture the join
/// ladder refuses by name; malformed refuses the mount (never a silent
/// default).
pub fn bindFromEnv(env: *const std.process.Environ.Map) endpoint.ParseError!endpoint.Bind {
    return endpoint.Bind.parse(knobs.present(env.get("SQUEEZEFS_MW_BIND")));
}

pub const Options = struct {
    /// The process environment (the explicit cap knob); null = derived.
    env: ?*const std.process.Environ.Map = null,
    /// The RAW cpu count (never fleet-share-divided).
    raw_cpus: u32,
    /// The `RLIMIT_NOFILE` in force (`raiseNofile()`).
    nofile: u64,
    /// The channel class; null = the process posture (`tls.posture()`,
    /// installed at bring-up). A set configured `mutual_tls` refuses a
    /// Plaintext peer loud at the handshake and counts it.
    posture: ?tls.Posture = null,
};

pub const StartError = error{ BindOff, OutOfMemory, ThreadSpawnFailed } || net.IpAddress.ListenError || endpoint.ParseError;

const Conn = struct {
    listener: *Listener,
    /// The raw socket (the stop nudge's and the sever's target).
    link: session.TcpLink,
    /// The TLS record layer over it under `mutual_tls`.
    tls_conn: ?*tls.Conn = null,
    sess: session.Session,
    id: u64,
    slot: usize,
    peer: ?verbs.WireIdentity = null,

    /// The byte stream the session reads and writes.
    fn stream(self: *Conn) session.Link {
        if (self.tls_conn) |c| return c.link();
        return self.link.link();
    }
};

pub const Listener = struct {
    gpa: std.mem.Allocator,
    io: Io,
    svc: *service.Service,
    gate: auth.Gate,
    server: net.Server,
    advertised: endpoint.EndpointWire,
    max_conns: u32,
    posture: tls.Posture,
    /// Live connections — the futex word `stop` waits on.
    live: futex.Word = .init(0),
    stopping: std.atomic.Value(bool) = .init(false),
    accept_thread: ?std.Thread = null,
    conns: []?*Conn,
    conns_lock: std.atomic.Value(bool) = .init(false),
    next_id: std.atomic.Value(u64) = .init(1),
    /// An ADMITTED connection ended (EOF, idle, a framing failure, the stop
    /// nudge): the job coordinator expires the session's shard leases now
    /// and the last enrolled worker's departure releases the WERO. Bound
    /// after `start` by the owner; read with acquire at each departure
    /// (pairs with `bindDeparture`'s release store).
    departure: std.atomic.Value(?*const Departure) = .init(null),
    retire_station: ?RetireStation = null,

    pub const Departure = struct {
        ctx: *anyopaque,
        call: *const fn (ctx: *anyopaque, session_id: u64) void,
    };

    pub fn bindDeparture(self: *Listener, d: ?*const Departure) void {
        self.departure.store(d, .release);
    }

    /// The retire's station seam (null in the product; set before the first
    /// connection): `decremented` runs on the retiring thread right after
    /// its decrement — from there a concurrent `stop` may free this
    /// listener — and `retired` after its last act. The use-after-free
    /// pin's rendezvous.
    pub const RetireStation = struct {
        ctx: *anyopaque,
        decremented: *const fn (ctx: *anyopaque) void,
        retired: *const fn (ctx: *anyopaque) void,
    };

    /// Bind and start accepting. `bind` is the ONE bind knob's posture.
    pub fn start(gpa: std.mem.Allocator, io: Io, bind: endpoint.Bind, svc: *service.Service, secret: auth.Secret, opts: Options) StartError!*Listener {
        const explicit = if (opts.env) |env| knobs.optIntKnob("SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS", u32, env) else null;
        const resolved = resolveMaxConnections(explicit, opts.raw_cpus, opts.nofile);
        if (resolved.railed) log.warn("cluster wire: SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS={d} railed to {d} by the fd ceiling (RLIMIT_NOFILE {d} / {d})", .{ explicit.?, resolved.value, opts.nofile, CONNECTION_FDS * LISTENER_FD_SHARE });
        const addr: net.IpAddress = switch (bind) {
            .off => return error.BindOff,
            .auto => .{ .ip4 = .unspecified(0) },
            .explicit => |e| e.toIp() orelse return error.MalformedEndpoint,
        };
        const self = try gpa.create(Listener);
        errdefer gpa.destroy(self);
        const conns = try gpa.alloc(?*Conn, resolved.value);
        errdefer gpa.free(conns);
        @memset(conns, null);
        var gate = try auth.Gate.init(gpa, secret, resolved.value);
        errdefer gate.deinit(gpa);
        var server = try net.IpAddress.listen(&addr, io, .{ .reuse_address = true });
        errdefer server.deinit(io);
        // THE endpoint law: an explicit bind is advertised VERBATIM (with
        // the bound port); `auto` advertises the default route's address.
        const bound_port = server.socket.address.getPort();
        var adv: endpoint.EndpointWire = switch (bind) {
            .explicit => |e| e,
            else => endpoint.EndpointWire.fromIp(defaultRouteIp(bound_port)),
            .off => unreachable,
        };
        adv.port = bound_port;
        const posture = opts.posture orelse tls.posture();
        self.* = .{ .gpa = gpa, .io = io, .svc = svc, .gate = gate, .server = server, .advertised = adv, .max_conns = resolved.value, .posture = posture, .conns = conns };
        self.accept_thread = std.Thread.spawn(.{}, acceptMain, .{self}) catch return error.ThreadSpawnFailed;
        log.info("cluster wire: listening at {f} (cap {d} connections, channel {t})", .{ adv, resolved.value, posture.channel() });
        return self;
    }

    /// Stop accepting, nudge every connection, wait for the last thread.
    pub fn stop(self: *Listener) void {
        self.stopping.store(true, .release);
        // `shutdown(SHUT_RDWR)` on the listening socket wakes a parked
        // `accept` with EINVAL (SocketNotListening) — the accept thread's
        // exit signal; a `close` alone leaves it parked.
        _ = linux.shutdown(self.server.socket.handle, linux.SHUT.RDWR);
        if (self.accept_thread) |t| t.join();
        self.server.socket.close(self.io);
        self.lockConns();
        for (self.conns) |c| if (c) |conn| conn.link.link().shutdown();
        self.unlockConns();
        while (self.live.load(.acquire) != 0) futex.wait(&self.live, self.live.load(.acquire));
        self.gate.deinit(self.gpa);
        self.gpa.free(self.conns);
        self.gpa.destroy(self);
    }

    pub fn liveConnections(self: *const Listener) u32 {
        return self.live.load(.acquire);
    }

    /// Test seam (the kill matrix's connection death): sever every live
    /// connection's socket from the serving side — the peers see EOF or a
    /// reset mid-session, exactly as a crashed holder's would.
    pub fn severAllConnections(self: *Listener) u32 {
        self.lockConns();
        defer self.unlockConns();
        var n: u32 = 0;
        for (self.conns) |c| if (c) |conn| {
            conn.link.link().shutdown();
            n += 1;
        };
        return n;
    }

    fn lockConns(self: *Listener) void {
        while (self.conns_lock.swap(true, .acquire)) std.atomic.spinLoopHint();
    }
    fn unlockConns(self: *Listener) void {
        self.conns_lock.store(false, .release);
    }

    /// `ConnGate.try_admit`: CAS the live count under the cap; past it the
    /// peer costs one accept + one close.
    fn tryAdmit(self: *Listener) ?usize {
        while (true) {
            const n = self.live.load(.acquire);
            if (n >= self.max_conns) return null;
            if (self.live.cmpxchgWeak(n, n + 1, .acq_rel, .acquire) == null) break;
        }
        self.lockConns();
        defer self.unlockConns();
        for (self.conns, 0..) |c, i| if (c == null) return i;
        unreachable; // live < max ⇒ a slot is free
    }

    /// The connection leaves the registry BEFORE its socket closes (so the
    /// stop nudge never touches a closed fd) and the live count drops
    /// AFTER everything it owned is gone (so `stop` frees nothing in use).
    fn unregister(self: *Listener, conn: *Conn) void {
        self.lockConns();
        self.conns[conn.slot] = null;
        self.unlockConns();
    }

    /// The connection's last act. Everything it needs of the listener is
    /// read BEFORE the decrement: from the decrement on, `stop` may see 0
    /// and free the listener, so the gauge takes the decrement's own result
    /// and the wake names the word by its address alone (a private futex
    /// is keyed by the address; the kernel reads nothing there — the
    /// waker-after-free shape every futex-backed release has).
    fn retire(self: *Listener) void {
        const station = self.retire_station;
        const word = &self.live;
        const prev = word.fetchSub(1, .acq_rel);
        if (station) |st| st.decremented(st.ctx);
        stats.cluster_wire_live_connections.set(prev - 1);
        futex.wakeAll(word);
        if (station) |st| st.retired(st.ctx);
    }

    fn acceptMain(self: *Listener) void {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, ACCEPT_THREAD_NAME);
        var backoff_ms: u64 = ACCEPT_BACKOFF_MIN_MS;
        while (!self.stopping.load(.acquire)) {
            const stream = self.server.accept(self.io) catch |e| switch (e) {
                error.SocketNotListening, error.Canceled => return,
                else => {
                    stats.cluster_wire_accept_backoffs.inc();
                    log.warn("cluster wire: accept failed ({t}) — backing off {d} ms", .{ e, backoff_ms });
                    self.io.sleep(.fromMilliseconds(@intCast(backoff_ms)), .awake) catch {};
                    backoff_ms = @min(backoff_ms * 2, ACCEPT_BACKOFF_MAX_MS);
                    continue;
                },
            };
            backoff_ms = ACCEPT_BACKOFF_MIN_MS;
            session.TcpLink.setNoDelay(stream);
            const slot = self.tryAdmit() orelse {
                stats.cluster_wire_connections_refused.inc();
                stream.close(self.io);
                continue;
            };
            stats.cluster_wire_live_connections.set(self.live.load(.acquire));
            const conn = self.gpa.create(Conn) catch {
                stream.close(self.io);
                self.retire();
                continue;
            };
            // The session (and under `mutual_tls` the record layer) is set
            // up on the connection's own thread: the TLS handshake is that
            // thread's first act, never the accept thread's.
            conn.* = .{ .listener = self, .link = .{ .io = self.io, .stream = stream }, .sess = undefined, .id = self.next_id.fetchAdd(1, .monotonic), .slot = slot };
            self.lockConns();
            self.conns[slot] = conn;
            self.unlockConns();
            const t = std.Thread.spawn(.{}, connMain, .{conn}) catch {
                self.unregister(conn);
                stream.close(self.io);
                self.gpa.destroy(conn);
                self.retire();
                continue;
            };
            t.detach();
        }
    }
};

fn nowMs() u64 {
    return clock.monotonicNs() / std.time.ns_per_ms;
}

/// The connection thread: under `mutual_tls` the TLS handshake first (a
/// refusal is counted by class and logged loud — a Plaintext peer, a
/// foreign CA, an expired leaf — and the socket closes), then the ladder
/// under the handshake deadline with the session key bound to the
/// channel's exporter, then the request loop until EOF, an idle timeout, a
/// framing failure or the listener's stop nudge.
fn connMain(conn: *Conn) void {
    var name_buf: thread_name.CommBuf = undefined;
    _ = thread_name.nameCurrentThread(&name_buf, CONN_THREAD_NAME);
    const l = conn.listener;
    defer {
        l.unregister(conn);
        if (conn.tls_conn) |c| {
            c.link().close();
            c.destroy(l.gpa);
        } else conn.link.link().close();
        l.gpa.destroy(conn);
        l.retire();
    }
    var binding: [tls.BINDING_BYTES]u8 = undefined;
    var binding_len: usize = 0;
    switch (l.posture) {
        .plaintext => {},
        .mutual_tls => |identity| {
            // The product's record layer is the kernel's (`RecordLayer.ktls`
            // over this socket); the handshake rides the userspace layer.
            conn.tls_conn = tls.serverHandshake(l.gpa, conn.link.link(), identity, l.io, .{ .record_layer = .ktls, .socket = conn.link.stream.socket.handle }) catch |e| {
                tls.countRefusal(e);
                stats.cluster_wire_admissions_refused.inc();
                log.warn("cluster wire: connection {d} refused at the TLS handshake: {s}", .{ conn.id, tls.refusalText(e) });
                // The silent peer is a Plaintext dialer parked on the
                // Challenge: answer it in ITS framing so its log names
                // the class instead of a bare dial timeout.
                if (e == error.PeerSilent) refusePlaintextPeer(conn);
                return;
            };
            binding = conn.tls_conn.?.binding();
            binding_len = binding.len;
        },
    }
    conn.sess = session.Session.init(l.gpa, conn.stream(), .coordinator, conn.id) catch return;
    defer conn.sess.deinit();
    if (!handshake(conn, binding[0..binding_len])) return;
    defer if (l.departure.load(.acquire)) |d| d.call(d.ctx, conn.id);
    while (!l.stopping.load(.acquire)) {
        const in = conn.sess.recvFrame(session.SESSION_IDLE_TIMEOUT_MS) catch |e| {
            switch (e) {
                error.Eof, error.Timeout => {},
                else => log.warn("cluster wire: connection {d} closed on {t}", .{ conn.id, e }),
            }
            return;
        };
        if (in.header.isReply()) return; // a reply on the serving half is a protocol error
        const t_serve = clock.monotonicNs();
        const served = l.svc.serve(in.header, in.payload, conn.peer, conn.id, conn.sess.payloadWindow(), conn.sess.scratch);
        const serve_ms = (clock.monotonicNs() -| t_serve) / std.time.ns_per_ms;
        if (serve_ms >= SLOW_SERVE_WARN_MS) {
            stats.cluster_wire_slow_serves.inc();
            log.warn("cluster wire: connection {d}: {s} served in {d} ms — every call multiplexed on this session waited behind it", .{ conn.id, verbs.Verb.nameOf(in.header.verb), serve_ms });
        }
        const rh = conn.sess.framer.nextHeaderRaw(in.header.verb, in.header.schema, served.status, in.header.correlation, true);
        conn.sess.sendSealed(rh, served.payload.len) catch return;
    }
}

/// A plaintext `Refused` handshake frame straight onto the raw socket (no
/// session: the peer never got a Challenge and never will).
fn refusePlaintextPeer(conn: *Conn) void {
    const text = "this channel is MutualTls (set SQUEEZEFS_WIRE_CHANNEL=mutual_tls and install the node's certificate)";
    var buf: [frame.HEADER_LEN + frame.HANDSHAKE_CAP]u8 = undefined;
    var b: codec.Builder = .init(buf[frame.HEADER_LEN..]);
    codec.encodePayload(verbs.Refused, .{ .head = .{ .reason_len = text.len, .reserved = @splat(0) }, .bytes = text }, &b) catch return;
    var framer: session.Framer = .init(.coordinator);
    const n = framer.seal(framer.handshakeHeader(.refused), &buf, b.pos) catch return;
    conn.link.link().writeAll(buf[0..n]) catch {};
}

/// Steps 1–5 on the coordinator's side (`binding` = the channel's
/// exporter under `mutual_tls`, empty on plaintext); false ⇒ the
/// connection closes. A plaintext peer's first bytes at a `mutual_tls`
/// listener never reach here (the record layer refused them); a TLS
/// peer's at a plaintext listener fail the frame header's magic screen.
fn handshake(conn: *Conn, binding: []const u8) bool {
    const l = conn.listener;
    const ch = l.gate.challenge(l.io, nowMs()) catch return false;
    stats.cluster_wire_outstanding_challenges.set(l.gate.nonces.liveCount());
    conn.sess.sendHandshake(verbs.Challenge, .challenge, ch) catch return false;
    const in = conn.sess.recvFrame(session.DIAL_TIMEOUT_MS) catch |e| {
        if (e == error.HeaderRefused and l.posture == .plaintext) {
            stats.cluster_wire_admissions_refused.inc();
            log.warn("cluster wire: connection {d} refused: not a plaintext frame (a MutualTls peer at a plaintext channel?)", .{conn.id});
        }
        return false;
    };
    if (in.header.verb != @backingInt(verbs.Verb.prove)) return false;
    const prove = codec.decodePayload(verbs.Prove, in.payload) catch return false;
    switch (l.gate.verify(prove, binding, nowMs())) {
        .admitted => |adm| {
            stats.cluster_wire_outstanding_challenges.set(l.gate.nonces.liveCount());
            conn.sess.sendHandshake(verbs.Admitted, .admitted, .{ .schema = verbs.CLUSTER_WIRE_SCHEMA, .reserved = 0 }) catch return false;
            // The class the session is admitted on is the CHANNEL's word:
            // confidential only with both kernel halves installed.
            conn.sess.framer.admitOn(adm.session_key, if (conn.tls_conn) |c| (if (c.confidential()) tls.Channel.mutual_tls else .plaintext) else .plaintext);
            conn.peer = adm.member();
            stats.cluster_wire_sessions_admitted.inc();
            return true;
        },
        .refused => |why| {
            stats.cluster_wire_outstanding_challenges.set(l.gate.nonces.liveCount());
            stats.cluster_wire_admissions_refused.inc();
            const text = why.text();
            conn.sess.sendHandshake(verbs.Refused, .refused, .{ .head = .{ .reason_len = @intCast(text.len), .reserved = @splat(0) }, .bytes = text }) catch {};
            log.warn("cluster wire: connection {d} refused at the handshake: {s}", .{ conn.id, text });
            return false;
        },
    }
}

/// The default route's address: a UDP socket "connected" to TEST-NET-1
/// (192.0.2.1 — reserved for documentation, never routed to a host, so
/// the connect is a route lookup and nothing is sent); loopback when the
/// host has no route. Raw syscalls: a datagram connect is not in the
/// `Io` surface and this runs once at start.
pub fn defaultRouteIp(port: u16) net.IpAddress {
    const fd_rc = linux.socket(linux.AF.INET, linux.SOCK.DGRAM | linux.SOCK.CLOEXEC, 0);
    if (linux.errno(fd_rc) != .SUCCESS) return .{ .ip4 = .loopback(port) };
    const fd: linux.fd_t = @intCast(fd_rc);
    defer _ = linux.close(fd);
    const target: linux.sockaddr.in = .{ .port = std.mem.nativeToBig(u16, 9), .addr = std.mem.bytesToValue(u32, &[4]u8{ 192, 0, 2, 1 }) };
    if (linux.errno(linux.connect(fd, &target, @sizeOf(linux.sockaddr.in))) != .SUCCESS) return .{ .ip4 = .loopback(port) };
    var local: linux.sockaddr.in = undefined;
    var len: linux.socklen_t = @sizeOf(linux.sockaddr.in);
    if (linux.errno(linux.getsockname(fd, @ptrCast(&local), &len)) != .SUCCESS) return .{ .ip4 = .loopback(port) };
    const bytes = std.mem.toBytes(local.addr);
    if (std.mem.allEqual(u8, &bytes, 0)) return .{ .ip4 = .loopback(port) };
    return .{ .ip4 = .{ .bytes = bytes, .port = port } };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "the connection cap derives from RAW cpus with the floor and the fd ceiling; an explicit cap wins verbatim, railed by the fd ceiling only" {
    try testing.expectEqual(@as(u32, 64), deriveMaxConnections(1, 1 << 20));
    try testing.expectEqual(@as(u32, 64), deriveMaxConnections(4, 1 << 20));
    try testing.expectEqual(@as(u32, 512), deriveMaxConnections(32, 1 << 20));
    // 32 cores but nofile 2048 ⇒ ceiling 256.
    try testing.expectEqual(@as(u32, 256), deriveMaxConnections(32, 2048));
    // A tiny nofile: the floor wins the tie (the operator raises nofile).
    try testing.expectEqual(@as(u32, 64), deriveMaxConnections(1, 64));
    try testing.expectEqual(Resolved{ .value = 1000, .railed = false }, resolveMaxConnections(1000, 1, 1 << 20));
    try testing.expectEqual(Resolved{ .value = 256, .railed = true }, resolveMaxConnections(1000, 1, 2048));
    try testing.expectEqual(Resolved{ .value = 512, .railed = false }, resolveMaxConnections(null, 32, 1 << 20));
    // The registry's range (64..=65536) is the knob's; the derivation never reads it.
    try testing.expectEqual(@as(i128, 64), knobs.row("SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS").kind.int.lo);
}

test "the default route lookup answers an address (loopback when the venue has no route); the bind knob's three postures; `off` refuses the start" {
    const ip = defaultRouteIp(7);
    try testing.expectEqual(@as(u16, 7), ip.getPort());
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(endpoint.Bind.auto, try bindFromEnv(&env));
    try env.put("SQUEEZEFS_MW_BIND", "off");
    try testing.expectEqual(endpoint.Bind.off, try bindFromEnv(&env));
    try env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    try testing.expectEqual(@as(u16, 0), (try bindFromEnv(&env)).explicit.port);
    try env.put("SQUEEZEFS_MW_BIND", "garbage");
    try testing.expectError(error.MalformedEndpoint, bindFromEnv(&env));
    var svc: service.Service = .init;
    try testing.expectError(error.BindOff, Listener.start(testing.allocator, testing.io, .off, &svc, @splat(0), .{ .raw_cpus = 1, .nofile = 1024 }));
}
