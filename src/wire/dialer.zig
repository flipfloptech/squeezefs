//! The dialer and the multiplexed session (`docs/porting/sym_wire_
//! membership_recovery.md` §1.5): one TCP dial under `DIAL_TIMEOUT`, the
//! ladder's peer side, the BOUNDED retry on a listener that closed before
//! its challenge (its connection cap or a shutdown) ending in the typed
//! `ListenerRefused`, the `deadOnArrival` peek, and `Mux` — one dialed
//! session split into a write half under a mutex (the MAC sequence is
//! per direction) and a dedicated reader thread `sqz-wire-mux`
//! demultiplexing replies by correlation onto a FIXED pool of parked
//! cells (K in flight costs ONE connection and zero per-call allocation;
//! decision 0002 row 228).
//!
//! Deadlines are the LEASE CLOCK's, never a per-op wrapper that drops a
//! handler: a call's reply bound is `DIAL_TIMEOUT` (10 s — the shortest
//! session's lifetime and the renewal beat's class, so a holder that has
//! not answered in a beat is not serving), after which the CALLER sees
//! `HolderUnreachable` (EAGAIN) and the holder's handler is never
//! interrupted — its late reply is dropped by correlation (`cluster_wire_
//! late_replies`). A transport failure on either half POISONS the whole
//! session and fails every parked call; the owning `Client` re-dials the
//! CURRENT endpoint on its next call (a failover moved the manager).
//!
//! Linux only (the reader thread's name, the futex cells, the peek).

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const net = Io.net;
const linux = std.os.linux;
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const thread_name = @import("../core/thread_name.zig");
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
    if (builtin.os.tag != .linux) @compileError("wire/dialer is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const MUX_THREAD_NAME = "sqz-wire-mux";

comptime {
    std.debug.assert(thread_name.fitsBudget(MUX_THREAD_NAME));
}

/// The retry ladder's first step: the finest grain at which a freed
/// connection slot can be observed (the listener releases a slot when a
/// connection thread exits; 100 ms is the accept tick the reference's
/// shutdown latch paced at — a dial that retried faster only re-paid the
/// accept + close).
pub const DIAL_RETRY_BASE_MS: u64 = 100;
/// The reply bound (module doc): one bound, the handshake's and the call's.
pub const CALL_REPLY_BOUND_MS: u32 = session.DIAL_TIMEOUT_MS;
/// Calls in flight per session: one lane's widest burst per conveyor pass
/// (the commit batch floor is 64 transactions), so a pipelining peer
/// never queues behind its own cells at the norm.
pub const MUX_IN_FLIGHT: u32 = 64;
/// Standing sessions a joined writer keeps (the manager wire, the
/// meta-ship lane's stop-and-wait + mux, the per-holder custody client,
/// the intents lane, the block-grant sink) — COUNTED IN CODE by the later
/// waves' census test against this constant.
pub const MEMBER_CONTROL_SESSIONS: u32 = 6;

/// `volumes × (depth + 1) + depth + MEMBER_CONTROL_SESSIONS`: what the
/// listener's cap is judged against per joined member.
pub fn memberSessionDemand(volumes: u32, depth: u32) u32 {
    return volumes * (depth + 1) + depth + MEMBER_CONTROL_SESSIONS;
}

/// One dial attempt's refusals, typed so the retry law can tell "the
/// listener accepted and closed" from everything else.
pub const DialError = error{
    /// EOF before any frame: the listener accepted and closed (its cap or a
    /// shutdown) — the ONE retryable dial outcome.
    BeforeChallenge,
    /// Nobody listening / unreachable / the dial timed out.
    Unreachable,
    /// The peer's gate refused our proof (or spoke another schema).
    HandshakeRefused,
    /// The peer answered with something that is not the ladder.
    Protocol,
    OutOfMemory,
    ThreadSpawnFailed,
};

fn nowMs() u64 {
    return clock.monotonicNs() / std.time.ns_per_ms;
}

/// `connect_timeout`: a non-blocking connect polled under `DIAL_TIMEOUT`
/// (the `Io`'s connect has no timeout arm; the kernel's own SYN ladder is
/// ~2 min — far past the bound), then the socket back to blocking for the
/// session's reads. Raw syscalls on the wire's OS thread.
pub fn connectTimeout(addr: net.IpAddress, timeout_ms: u32) error{Unreachable}!net.Stream {
    var storage: [128]u8 align(8) = undefined;
    var len: linux.socklen_t = undefined;
    var family: u32 = undefined;
    switch (addr) {
        .ip4 => |v4| {
            family = linux.AF.INET;
            const sa: *linux.sockaddr.in = @ptrCast(&storage);
            sa.* = .{ .port = std.mem.nativeToBig(u16, v4.port), .addr = std.mem.bytesToValue(u32, &v4.bytes) };
            len = @sizeOf(linux.sockaddr.in);
        },
        .ip6 => |v6| {
            family = linux.AF.INET6;
            const sa: *linux.sockaddr.in6 = @ptrCast(&storage);
            sa.* = .{ .port = std.mem.nativeToBig(u16, v6.port), .flowinfo = 0, .addr = v6.bytes, .scope_id = v6.interface.index };
            len = @sizeOf(linux.sockaddr.in6);
        },
    }
    const fd_rc = linux.socket(family, linux.SOCK.STREAM | linux.SOCK.CLOEXEC | linux.SOCK.NONBLOCK, 0);
    if (linux.errno(fd_rc) != .SUCCESS) return error.Unreachable;
    const fd: linux.fd_t = @intCast(fd_rc);
    errdefer _ = linux.close(fd);
    switch (linux.errno(linux.connect(fd, &storage, len))) {
        .SUCCESS => {},
        .INPROGRESS => {
            var pfd = [_]linux.pollfd{.{ .fd = fd, .events = linux.POLL.OUT, .revents = 0 }};
            const n = linux.poll(&pfd, 1, @intCast(timeout_ms));
            if (linux.errno(n) != .SUCCESS or n == 0) return error.Unreachable;
            var so_err: i32 = 0;
            var so_len: linux.socklen_t = @sizeOf(i32);
            if (linux.errno(linux.getsockopt(fd, linux.SOL.SOCKET, linux.SO.ERROR, @ptrCast(&so_err), &so_len)) != .SUCCESS or so_err != 0) return error.Unreachable;
        },
        else => return error.Unreachable,
    }
    const flags = linux.fcntl(fd, linux.F.GETFL, 0);
    if (linux.errno(flags) != .SUCCESS) return error.Unreachable;
    const nonblock: usize = @as(u32, @bitCast(linux.O{ .NONBLOCK = true }));
    if (linux.errno(linux.fcntl(fd, linux.F.SETFL, flags & ~nonblock)) != .SUCCESS) return error.Unreachable;
    return .{ .socket = .{ .handle = fd, .address = addr } };
}

/// One attempt: connect, under `mutual_tls` the TLS handshake (a refusal
/// is loud: the listener's CA is not ours, our leaf is not under its CA,
/// a Plaintext listener answered), the ladder's peer side with the key
/// bound to the channel's exporter, the mux stood up.
pub fn connectOnce(gpa: std.mem.Allocator, io: Io, ep: endpoint.EndpointWire, secret: *const auth.Secret, peer_id: []const u8, posture: tls.Posture) DialError!*Mux {
    const addr = ep.toIp() orelse return error.Unreachable;
    const stream = try connectTimeout(addr, session.DIAL_TIMEOUT_MS);
    session.TcpLink.setNoDelay(stream);
    const mux = gpa.create(Mux) catch {
        stream.close(io);
        return error.OutOfMemory;
    };
    errdefer gpa.destroy(mux);
    mux.* = .{ .gpa = gpa, .io = io, .link = .{ .io = io, .stream = stream }, .sess = undefined, .cells = undefined };
    errdefer stream.close(io);
    var binding: [tls.BINDING_BYTES]u8 = undefined;
    var binding_len: usize = 0;
    switch (posture) {
        .plaintext => {},
        .mutual_tls => |identity| {
            mux.tls_conn = tls.clientHandshake(gpa, mux.link.link(), identity, io, .{}) catch |e| {
                stats.cluster_wire_tls_dial_refused.inc();
                return switch (e) {
                    error.Eof => error.BeforeChallenge,
                    error.Timeout, error.PeerSilent, error.Transport => error.Unreachable,
                    error.OutOfMemory => error.OutOfMemory,
                    else => blk: {
                        log.warn("cluster wire: peer at {f} refused at the TLS handshake: {s}", .{ ep, tls.refusalText(e) });
                        break :blk error.HandshakeRefused;
                    },
                };
            };
            binding = mux.tls_conn.?.binding();
            binding_len = binding.len;
        },
    }
    errdefer if (mux.tls_conn) |c| c.destroy(gpa);
    mux.sess = session.Session.init(gpa, mux.stream(), .peer, 0) catch return error.OutOfMemory;
    errdefer mux.sess.deinit();
    // Step 1: the coordinator speaks first. EOF here = BeforeChallenge.
    const ch_in = mux.sess.recvFrame(session.DIAL_TIMEOUT_MS) catch |e| return switch (e) {
        error.Eof => error.BeforeChallenge,
        error.HeaderRefused => blk: {
            // A frame that is not a frame: a MutualTls listener's TLS
            // alert/record where the Challenge should be.
            if (posture == .plaintext) log.warn("cluster wire: peer at {f} did not answer with a plaintext Challenge (a MutualTls listener at a plaintext dialer?)", .{ep});
            break :blk error.HandshakeRefused;
        },
        else => error.Unreachable,
    };
    if (ch_in.header.verb == @backingInt(verbs.Verb.refused)) {
        // A MutualTls listener answering a Plaintext dialer in its own
        // framing after the handshake bound: the refusal names the class.
        const r = codec.decodePayload(verbs.Refused, ch_in.payload) catch return error.Protocol;
        log.warn("cluster wire: peer at {f} refused the connection before its challenge: {s}", .{ ep, r.bytes });
        return error.HandshakeRefused;
    }
    if (ch_in.header.verb != @backingInt(verbs.Verb.challenge)) return error.Protocol;
    const ch = codec.decodePayload(verbs.Challenge, ch_in.payload) catch return error.Protocol;
    // Steps 2–4.
    const ans = auth.answer(io, secret, peer_id, ch, binding[0..binding_len]) catch |e| return switch (e) {
        error.SchemaMismatch => blk: {
            log.warn("cluster wire: peer at {f} speaks wire schema {d}, this binary {d} — refusing", .{ ep, ch.schema, verbs.CLUSTER_WIRE_SCHEMA });
            break :blk error.HandshakeRefused;
        },
        else => error.Unreachable,
    };
    mux.sess.sendHandshake(verbs.Prove, .prove, .{ .head = ans.prove_head, .bytes = peer_id }) catch return error.Unreachable;
    const verdict = mux.sess.recvFrame(session.DIAL_TIMEOUT_MS) catch return error.Unreachable;
    if (verdict.header.verb == @backingInt(verbs.Verb.refused)) {
        const r = codec.decodePayload(verbs.Refused, verdict.payload) catch return error.Protocol;
        log.warn("cluster wire: peer at {f} refused our proof: {s}", .{ ep, r.bytes });
        return error.HandshakeRefused;
    }
    if (verdict.header.verb != @backingInt(verbs.Verb.admitted)) return error.Protocol;
    _ = codec.decodePayload(verbs.Admitted, verdict.payload) catch return error.Protocol;
    mux.sess.framer.admitOn(ans.session_key, posture.channel());
    // Step 5: the mux's cells and its reader.
    mux.cells = gpa.alloc(Cell, MUX_IN_FLIGHT) catch return error.OutOfMemory;
    errdefer gpa.free(mux.cells);
    for (mux.cells) |*c| c.* = .{};
    mux.reader = std.Thread.spawn(.{}, Mux.readerMain, .{mux}) catch return error.ThreadSpawnFailed;
    return mux;
}

/// The bounded retry (`connect_sync`): repeat on `BeforeChallenge` with a
/// doubling backoff from the retry base until `DIAL_TIMEOUT` is spent —
/// past it `ListenerRefused` (EAGAIN), never an invalid-operation at the
/// first EOF ("the 14th member of the box's 32-member fleet died on that
/// word"). Other refusals map once: nobody listening ⇒ `HolderUnreachable`.
pub fn connect(gpa: std.mem.Allocator, io: Io, ep: endpoint.EndpointWire, secret: *const auth.Secret, peer_id: []const u8) errors.Error!*Mux {
    return connectWith(gpa, io, ep, secret, peer_id, tls.posture());
}

/// `connect` under an explicit channel posture (the process posture is
/// the product's; the tests pair classes freely).
pub fn connectWith(gpa: std.mem.Allocator, io: Io, ep: endpoint.EndpointWire, secret: *const auth.Secret, peer_id: []const u8, posture: tls.Posture) errors.Error!*Mux {
    const deadline = nowMs() + session.DIAL_TIMEOUT_MS;
    var backoff_ms: u64 = DIAL_RETRY_BASE_MS;
    while (true) {
        return connectOnce(gpa, io, ep, secret, peer_id, posture) catch |e| switch (e) {
            error.BeforeChallenge => {
                const now = nowMs();
                if (now >= deadline) {
                    stats.cluster_wire_listener_refused.inc();
                    return error.ListenerRefused;
                }
                stats.cluster_wire_dial_retries.inc();
                io.sleep(.fromMilliseconds(@intCast(@min(backoff_ms, deadline - now))), .awake) catch {};
                backoff_ms = @min(backoff_ms * 2, session.DIAL_TIMEOUT_MS);
                continue;
            },
            error.Unreachable, error.Protocol => return error.HolderUnreachable,
            error.HandshakeRefused => return error.HandshakeRefused,
            error.OutOfMemory => return error.OutOfMemory,
            error.ThreadSpawnFailed => return error.WorkerClosed,
        };
    }
}

pub const CallResult = struct { status: frame.Status, payload: []const u8 };

/// One in-flight call's cell: the state word (free → armed → delivering →
/// free), the generation that makes a correlation unique across reuse,
/// the caller's reply buffer, the futex the caller parks on.
const Cell = struct {
    const FREE: u32 = 0;
    const ARMED: u32 = 1;
    const DELIVERING: u32 = 2;

    state: std.atomic.Value(u32) = .init(FREE),
    generation: u32 = 0,
    out: []u8 = &.{},
    done: futex.Word = .init(0),
    status: u16 = 0,
    len: u32 = 0,
    err: ?errors.Error = null,
};

pub const Mux = struct {
    gpa: std.mem.Allocator,
    io: Io,
    /// The raw socket (the shutdown nudge's and the peek's target).
    link: session.TcpLink,
    /// The TLS record layer over it under `mutual_tls`.
    tls_conn: ?*tls.Conn = null,
    sess: session.Session,
    cells: []Cell,
    write_lock: Io.Mutex = .init,
    reader: std.Thread = undefined,
    poisoned: std.atomic.Value(bool) = .init(false),
    closing: std.atomic.Value(bool) = .init(false),

    pub fn isPoisoned(self: *const Mux) bool {
        return self.poisoned.load(.acquire);
    }

    /// The byte stream the session reads and writes.
    fn stream(self: *Mux) session.Link {
        if (self.tls_conn) |c| return c.link();
        return self.link.link();
    }

    /// The channel class this session was admitted on.
    pub fn channel(self: *const Mux) tls.Channel {
        return self.sess.framer.channel;
    }

    fn correlationOf(index: usize, generation: u32) u64 {
        return (@as(u64, generation) << 32) | @as(u64, @intCast(index));
    }

    fn acquireCell(self: *Mux) ?usize {
        for (self.cells, 0..) |*c, i| {
            if (c.state.cmpxchgStrong(Cell.FREE, Cell.ARMED, .acq_rel, .acquire) == null) return i;
        }
        return null;
    }

    /// Ship one call and park for its reply (bounded). The reply payload
    /// is copied into `out`; a reply wider than `out` is `ValueTooLarge`.
    pub fn call(self: *Mux, req: verbs.Request, out: []u8) errors.Error!CallResult {
        if (self.isPoisoned()) return error.ConnectionLost;
        const idx = self.acquireCell() orelse return error.QueueFull;
        const cell = &self.cells[idx];
        cell.generation +%= 1;
        cell.out = out;
        cell.done.store(0, .release);
        cell.err = null;
        const corr = correlationOf(idx, cell.generation);
        self.write_lock.lockUncancelable(self.io);
        const sent = self.sess.sendCall(req, corr);
        self.write_lock.unlock(self.io);
        sent catch {
            // Nothing left the socket whole: the session is broken, not
            // the call ambiguous.
            cell.state.store(Cell.FREE, .release);
            self.poison();
            return error.ConnectionLost;
        };
        const deadline = clock.monotonicNs() + @as(u64, CALL_REPLY_BOUND_MS) * std.time.ns_per_ms;
        while (cell.done.load(.acquire) == 0) {
            const now = clock.monotonicNs();
            if (now >= deadline) {
                // Give up unless the reader is already delivering.
                if (cell.state.cmpxchgStrong(Cell.ARMED, Cell.FREE, .acq_rel, .acquire) == null) {
                    stats.cluster_wire_call_timeouts.inc();
                    return error.HolderUnreachable;
                }
                while (cell.done.load(.acquire) == 0) futex.wait(&cell.done, 0);
                break;
            }
            _ = futex.waitTimeout(&cell.done, 0, deadline - now);
        }
        const result: CallResult = .{ .status = @fromBackingInt(@intCast(cell.status)), .payload = out[0..cell.len] };
        const err = cell.err;
        cell.state.store(Cell.FREE, .release);
        if (err) |e| return e;
        return result;
    }

    /// Ship one typed call and interpret the reply.
    pub fn callTyped(self: *Mux, req: verbs.Request, out: []u8) errors.Error!verbs.Reply {
        const r = try self.call(req, out);
        return service.interpretReply(@as(verbs.Verb, req), r.status, r.payload);
    }

    /// The reader: every reply to its cell by correlation; a transport
    /// failure poisons the session and fails every parked call.
    fn readerMain(self: *Mux) void {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, MUX_THREAD_NAME);
        while (!self.closing.load(.acquire)) {
            const in = self.sess.recvFrame(null) catch |e| {
                if (!self.closing.load(.acquire) and e != error.Eof) log.warn("cluster wire: mux session failed on {t} — poisoning", .{e});
                self.poison();
                return;
            };
            if (!in.header.isReply()) {
                self.poison();
                return;
            }
            self.deliver(in);
        }
    }

    fn deliver(self: *Mux, in: session.Incoming) void {
        const idx: usize = @intCast(in.header.correlation & 0xFFFF_FFFF);
        const gen: u32 = @intCast(in.header.correlation >> 32);
        if (idx >= self.cells.len) {
            stats.cluster_wire_late_replies.inc();
            return;
        }
        const cell = &self.cells[idx];
        if (cell.generation != gen or cell.state.cmpxchgStrong(Cell.ARMED, Cell.DELIVERING, .acq_rel, .acquire) != null) {
            stats.cluster_wire_late_replies.inc();
            return;
        }
        if (in.payload.len > cell.out.len) {
            cell.err = error.ValueTooLarge;
            cell.len = 0;
        } else {
            @memcpy(cell.out[0..in.payload.len], in.payload);
            cell.len = @intCast(in.payload.len);
            cell.status = in.header.status;
            if (frame.Status.fromWire(in.header.status) == null) cell.err = error.WireSchemaRefused;
        }
        cell.done.store(1, .release);
        futex.wakeAll(&cell.done);
    }

    /// Poison: every armed call fails `HolderUnreachable` (its frame may
    /// have reached the holder — the sent-then-lost ambiguity the caller's
    /// idempotent retry resolves against durable state).
    fn poison(self: *Mux) void {
        if (self.poisoned.swap(true, .acq_rel)) return;
        stats.cluster_wire_sessions_poisoned.inc();
        for (self.cells) |*cell| {
            if (cell.state.cmpxchgStrong(Cell.ARMED, Cell.DELIVERING, .acq_rel, .acquire) == null) {
                cell.err = error.HolderUnreachable;
                cell.len = 0;
                cell.done.store(1, .release);
                futex.wakeAll(&cell.done);
            }
        }
    }

    /// `dead_on_arrival`: a non-blocking peek before a ONE-ATTEMPT verb's
    /// single send — EOF queued (the idle reaper closed it) ⇒ dead; EAGAIN
    /// ⇒ alive; any other errno ⇒ broken. A poisoned session is dead.
    pub fn deadOnArrival(self: *Mux) bool {
        if (self.isPoisoned()) return true;
        var b: [1]u8 = undefined;
        const rc = linux.recvfrom(self.link.stream.socket.handle, &b, 1, linux.MSG.PEEK | linux.MSG.DONTWAIT, null, null);
        switch (linux.errno(rc)) {
            .SUCCESS => {
                if (rc == 0) {
                    stats.cluster_wire_dead_on_arrival.inc();
                    return true;
                }
                return false; // the reader will take it
            },
            .AGAIN => return false,
            else => {
                stats.cluster_wire_dead_on_arrival.inc();
                return true;
            },
        }
    }

    /// Nudge the reader, join it, close the socket, free everything.
    pub fn close(self: *Mux) void {
        self.closing.store(true, .release);
        self.link.link().shutdown();
        self.reader.join();
        self.poison();
        if (self.tls_conn) |c| {
            c.link().close();
            c.destroy(self.gpa);
        } else self.link.link().close();
        self.sess.deinit();
        self.gpa.free(self.cells);
        self.gpa.destroy(self);
    }
};

/// A verb client over one endpoint: re-dials a poisoned session on the
/// next call (the joined wire's `redial` law), counts the reconnects.
pub const Client = struct {
    gpa: std.mem.Allocator,
    io: Io,
    ep: endpoint.EndpointWire,
    secret: auth.Secret,
    identity: verbs.WireIdentity,
    mux: ?*Mux = null,
    /// null = the process posture.
    posture: ?tls.Posture = null,

    pub fn init(gpa: std.mem.Allocator, io: Io, ep: endpoint.EndpointWire, secret: auth.Secret, identity: verbs.WireIdentity) Client {
        return .{ .gpa = gpa, .io = io, .ep = ep, .secret = secret, .identity = identity };
    }

    pub fn deinit(self: *Client) void {
        if (self.mux) |m| m.close();
        self.mux = null;
    }

    fn ensure(self: *Client) errors.Error!*Mux {
        if (self.mux) |m| {
            if (!m.isPoisoned()) return m;
            m.close();
            self.mux = null;
            stats.cluster_wire_reconnects.inc();
        }
        const peer_id = self.identity.peerId();
        const m = try connectWith(self.gpa, self.io, self.ep, &self.secret, &peer_id, self.posture orelse tls.posture());
        self.mux = m;
        return m;
    }

    pub fn call(self: *Client, req: verbs.Request, out: []u8) errors.Error!CallResult {
        const m = try self.ensure();
        return m.call(req, out);
    }

    pub fn callTyped(self: *Client, req: verbs.Request, out: []u8) errors.Error!verbs.Reply {
        const m = try self.ensure();
        return m.callTyped(req, out);
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "the member session demand and the in-flight bound are the counted constants" {
    try testing.expectEqual(@as(u32, 6), MEMBER_CONTROL_SESSIONS);
    try testing.expectEqual(@as(u32, 1 * (2 + 1) + 2 + 6), memberSessionDemand(1, 2));
    try testing.expectEqual(@as(u32, 64), MUX_IN_FLIGHT);
    try testing.expectEqual(session.DIAL_TIMEOUT_MS, CALL_REPLY_BOUND_MS);
    try testing.expectEqual(@as(u64, 0x0000_0005_0000_0003), Mux.correlationOf(3, 5));
}

test "a dial to a port nobody listens on is HolderUnreachable (EAGAIN), never EIO" {
    const secret: auth.Secret = @splat(1);
    const id: verbs.WireIdentity = .{ .node_token = 1, .writer_id = 1, .mount_slot = 0, .reserved = 0 };
    // Bind a listener-less port to learn a free one, then close it.
    var probe = try net.IpAddress.listen(&.{ .ip4 = .loopback(0) }, testing.io, .{ .reuse_address = true });
    const port = probe.socket.address.getPort();
    probe.deinit(testing.io);
    const ep = endpoint.EndpointWire.fromIp(.{ .ip4 = .loopback(port) });
    const peer_id = id.peerId();
    try testing.expectError(error.HolderUnreachable, connect(testing.allocator, testing.io, ep, &secret, &peer_id));
    try testing.expectEqual(linux.E.AGAIN, errors.toErrno(error.HolderUnreachable));
}
