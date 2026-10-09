//! One authenticated cluster-wire session's framing state and its frame
//! I/O (`docs/porting/sym_wire_membership_recovery.md` §1.2 receive
//! discipline, §1.4–§1.5).
//!
//! `Framer` is the portable core both transports share: the per-direction
//! sequence words, the direction bit the role fixes, the session key once
//! admitted, and the ONE decode law — a frame is accepted only under the
//! expected `(dir, seq)` with a verifying MAC; a failure is a SESSION
//! error (the caller poisons the session), never a skipped frame. The
//! in-process loopback hub drives two `Framer`s back to back; a TCP
//! `Session` drives one over a `Link`.
//!
//! The receive discipline over a link: read 48 B → screen the header
//! (magic, version, flags, reserved, `len ≤ cap(verb class)`) → stream
//! the payload in `FRAME_CHUNK_BYTES` rounds under the BODY deadline (the
//! receive buffer grows one chunk per round, so a lying length costs one
//! chunk) → read the trailer → verify the checksum, then the MAC under
//! the expected `(dir, seq)`. The header wait is bounded by the SESSION
//! idle timeout (an idle authenticated peer legitimately waits between
//! frames); the body wait by the body timeout (a dribbling peer is an
//! error).
//!
//! The wire is TCP through the `Io` the daemon passes (`std.Io.net`), ON
//! PURPOSE not io_uring: network TCP/TLS stacks are charter §1.2's
//! sanctioned non-uring exception, and the wire's threads are OS threads
//! (charter §9 "thread-per-connection for wires"), so a blocking socket
//! read on them parks nothing but themselves.

const std = @import("std");
const Io = std.Io;
const net = Io.net;
const frame = @import("frame.zig");
const codec = @import("codec.zig");
const verbs = @import("verbs.zig");
const stats = @import("stats.zig");
const log = @import("../core/log.zig");

pub const Role = enum { coordinator, peer };

/// Socket timeouts (one bound, three uses — §1.5): the dial, the
/// handshake's reads and writes, and a call's reply bound. 10 s is the
/// lifetime of the shortest slot-holding session, so a dialer that
/// retried one bound has outlived every pre-authn straggler.
pub const DIAL_TIMEOUT_MS: u32 = 10_000;
/// A frame whose body is still arriving after this is a dribbling peer,
/// not a slow link (a 1 MiB control frame at 1 MB/s is 1 s).
pub const FRAME_BODY_TIMEOUT_MS: u32 = 30_000;
/// An authenticated session with no frame for this long is reaped by
/// its server (the dialer's `deadOnArrival` peek sees the EOF).
pub const SESSION_IDLE_TIMEOUT_MS: u32 = 60_000;

pub const FrameError = error{
    /// The peer closed (EOF at a frame boundary or inside one).
    Eof,
    Timeout,
    TransportFailed,
    HeaderRefused,
    ChecksumFailed,
    MacFailed,
    SequenceBroken,
    DirectionWrong,
    /// The payload does not fit the send buffer (a handler bug: every
    /// tail is capped below the control cap).
    Overflow,
};

pub const Incoming = struct { header: frame.Header, payload: []const u8 };

/// The portable framing core.
pub const Framer = struct {
    role: Role,
    key: ?frame.SessionKey = null,
    send_seq: u64 = 0,
    recv_seq: u64 = 0,

    pub fn init(role: Role) Framer {
        return .{ .role = role };
    }

    /// Step 5 of the ladder: from here every frame carries the MAC.
    pub fn admit(self: *Framer, key: frame.SessionKey) void {
        self.key = key;
        self.send_seq = 0;
        self.recv_seq = 0;
    }

    pub fn authenticated(self: *const Framer) bool {
        return self.key != null;
    }

    fn dirFlag(self: *const Framer) u16 {
        return if (self.role == .peer) frame.FLAG_DIR_PEER_TO_COORD else 0;
    }

    /// The header of the next outgoing session frame (the caller fills
    /// `len` through `seal`).
    pub fn nextHeader(self: *Framer, verb: verbs.Verb, status: frame.Status, correlation: u64, reply: bool) frame.Header {
        return self.nextHeaderRaw(@backingInt(verb), verb.schema(), status, correlation, reply);
    }

    /// The raw form: a reply to an UNKNOWN verb echoes the verb word and
    /// the schema the call carried (so the caller's demux still matches).
    pub fn nextHeaderRaw(self: *Framer, verb_word: u16, schema: u32, status: frame.Status, correlation: u64, reply: bool) frame.Header {
        std.debug.assert(self.key != null);
        const seq = self.send_seq;
        self.send_seq += 1;
        return .{
            .flags = frame.FLAG_MAC_PRESENT | self.dirFlag() | (if (reply) frame.FLAG_REPLY else 0),
            .len = 0,
            .verb = verb_word,
            .status = @backingInt(status),
            .correlation = correlation,
            .seq = seq,
            .schema = schema,
        };
    }

    /// A MAC-less handshake frame's header (seq 0, the direction bit set).
    pub fn handshakeHeader(self: *const Framer, verb: verbs.Verb) frame.Header {
        std.debug.assert(verb.isHandshake());
        return .{ .flags = self.dirFlag(), .len = 0, .verb = @backingInt(verb), .status = 0, .correlation = 0, .seq = 0, .schema = verb.schema() };
    }

    /// Seal a frame IN PLACE: `buf[HEADER_LEN..][0..payload_len]` already
    /// holds the payload; the header is stamped at 0 and the MAC (when the
    /// header says so) after the payload. Returns the frame's length.
    pub fn seal(self: *const Framer, h: frame.Header, buf: []u8, payload_len: usize) FrameError!usize {
        const mac_len: usize = if (h.macPresent()) frame.MAC_BYTES else 0;
        const total = frame.HEADER_LEN + payload_len + mac_len;
        if (buf.len < total) return error.Overflow;
        const payload = buf[frame.HEADER_LEN..][0..payload_len];
        frame.encodeHeader(h, payload, buf[0..frame.HEADER_LEN]);
        if (h.macPresent()) frame.computeMac(&self.key.?, buf[0..frame.HEADER_LEN], payload, buf[frame.HEADER_LEN + payload_len ..][0..frame.MAC_BYTES]);
        stats.cluster_wire_frames_sent.inc();
        stats.cluster_wire_bytes_sent.add(total);
        return total;
    }

    /// The cap a header's verb class allows before any payload is read.
    pub fn capFor(verb_word: u16) u32 {
        if (verbs.Verb.fromWire(verb_word)) |v| if (v.isHandshake()) return frame.HANDSHAKE_CAP;
        return frame.CONTROL_CAP;
    }

    /// The ONE acceptance law over an in-memory frame (the loopback path;
    /// the link path runs the same checks piecewise in `Session.recvFrame`).
    pub fn accept(self: *Framer, bytes: []const u8) FrameError!Incoming {
        if (bytes.len < frame.HEADER_LEN) return error.Eof;
        const hb = bytes[0..frame.HEADER_LEN];
        const verb_word = std.mem.readInt(u16, hb[12..14], .little);
        const h = switch (frame.decodeHeader(hb, capFor(verb_word))) {
            .ok => |h| h,
            .refused => {
                stats.cluster_wire_header_refusals.inc();
                return error.HeaderRefused;
            },
        };
        const mac_len: usize = if (h.macPresent()) frame.MAC_BYTES else 0;
        if (bytes.len != frame.HEADER_LEN + @as(usize, h.len) + mac_len) return error.Eof;
        const payload = bytes[frame.HEADER_LEN..][0..h.len];
        const mac: ?*const [frame.MAC_BYTES]u8 = if (h.macPresent()) bytes[frame.HEADER_LEN + h.len ..][0..frame.MAC_BYTES] else null;
        try self.verify(hb, h, payload, mac);
        return .{ .header = h, .payload = payload };
    }

    /// Checksum, then the MAC under the expected `(dir, seq)`; a
    /// handshake frame (MAC-less, seq 0) is accepted only while the session
    /// is unauthenticated; a session frame only once it is.
    pub fn verify(self: *Framer, hb: *const [frame.HEADER_LEN]u8, h: frame.Header, payload: []const u8, mac: ?*const [frame.MAC_BYTES]u8) FrameError!void {
        if (!frame.verifyChecksum(hb, payload)) return error.ChecksumFailed;
        const expect_dir = self.role == .coordinator; // the peer's frames carry the bit
        if (h.peerToCoord() != expect_dir) return error.DirectionWrong;
        const handshake = if (verbs.Verb.fromWire(h.verb)) |v| v.isHandshake() else false;
        if (self.key) |*k| {
            if (handshake or !h.macPresent()) {
                stats.cluster_wire_mac_failures.fire("session frame without a MAC");
                return error.MacFailed;
            }
            if (!frame.verifyMac(k, hb, payload, mac.?)) {
                stats.cluster_wire_mac_failures.fire("per-frame MAC invalid");
                return error.MacFailed;
            }
            if (h.seq != self.recv_seq) return error.SequenceBroken;
            self.recv_seq += 1;
        } else {
            if (!handshake or h.macPresent() or h.seq != 0) return error.SequenceBroken;
        }
        stats.cluster_wire_frames_received.inc();
        stats.cluster_wire_bytes_received.add(frame.HEADER_LEN + payload.len + @as(usize, if (mac != null) frame.MAC_BYTES else 0));
    }
};

// ---------------------------------------------------------------------------
// The link and the session over it.
// ---------------------------------------------------------------------------

/// A byte stream the session reads and writes: TCP today, mTLS at rung 8.
pub const Link = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// Fill `buf` whole; `timeout_ms` null = wait forever.
        readExact: *const fn (ctx: *anyopaque, buf: []u8, timeout_ms: ?u32) FrameError!void,
        writeAll: *const fn (ctx: *anyopaque, bytes: []const u8) FrameError!void,
        /// Nudge a parked reader (`shutdown(SHUT_RDWR)`).
        shutdown: *const fn (ctx: *anyopaque) void,
        close: *const fn (ctx: *anyopaque) void,
    };

    pub fn readExact(self: Link, buf: []u8, timeout_ms: ?u32) FrameError!void {
        return self.vtable.readExact(self.ctx, buf, timeout_ms);
    }
    pub fn writeAll(self: Link, bytes: []const u8) FrameError!void {
        return self.vtable.writeAll(self.ctx, bytes);
    }
    pub fn shutdown(self: Link) void {
        self.vtable.shutdown(self.ctx);
    }
    pub fn close(self: Link) void {
        self.vtable.close(self.ctx);
    }
};

/// A TCP stream as a `Link` (the `Io`'s blocking posix path on an OS
/// thread; `TCP_NODELAY` set by the dialer and the listener so a
/// pipelined session's back-to-back frames never wait for a delayed ACK —
/// measured 40 ms steps per reply without it).
pub const TcpLink = struct {
    io: Io,
    stream: net.Stream,

    pub fn link(self: *TcpLink) Link {
        return .{ .ctx = self, .vtable = &vtable };
    }

    pub fn setNoDelay(stream: net.Stream) void {
        const one: c_int = 1;
        std.posix.setsockopt(stream.socket.handle, std.posix.IPPROTO.TCP, std.posix.TCP.NODELAY, std.mem.asBytes(&one)) catch |e| {
            log.warn("cluster wire: TCP_NODELAY refused ({t}) — pipelined replies will pace at the delayed-ACK step", .{e});
        };
    }

    const vtable: Link.VTable = .{ .readExact = readExact, .writeAll = writeAll, .shutdown = shutdown, .close = close };

    /// The socket's reads and writes are the kernel's own calls (the wire
    /// is the TCP stack's by design, charter §1.2 — never an `Io`
    /// operation: a lane fiber dials a holder lazily from a handler lane
    /// and the mount's root task dials the manager, and an `Io` read
    /// there is the forbidden blocking class; the bound is a `poll(2)`
    /// before each read, since an `Io` timeout would be a `Batch`).
    fn readExact(ctx: *anyopaque, buf: []u8, timeout_ms: ?u32) FrameError!void {
        const self: *TcpLink = @ptrCast(@alignCast(ctx));
        const fd = self.stream.socket.handle;
        var filled: usize = 0;
        while (filled < buf.len) {
            if (timeout_ms) |ms| {
                var pfd = [_]std.os.linux.pollfd{.{ .fd = fd, .events = std.os.linux.POLL.IN, .revents = 0 }};
                const n = std.os.linux.poll(&pfd, 1, @intCast(ms));
                if (std.os.linux.errno(n) != .SUCCESS) return error.TransportFailed;
                if (n == 0) return error.Timeout;
            }
            const rc = std.os.linux.recvfrom(fd, buf[filled..].ptr, buf.len - filled, 0, null, null);
            switch (std.os.linux.errno(rc)) {
                .SUCCESS => {},
                .INTR => continue,
                else => return error.TransportFailed,
            }
            if (rc == 0) return error.Eof;
            filled += @intCast(rc);
        }
    }

    fn writeAll(ctx: *anyopaque, bytes: []const u8) FrameError!void {
        const self: *TcpLink = @ptrCast(@alignCast(ctx));
        const fd = self.stream.socket.handle;
        var sent: usize = 0;
        while (sent < bytes.len) {
            // NOSIGNAL: a peer that closed answers EPIPE here, never a
            // signal to the daemon.
            const rc = std.os.linux.sendto(fd, bytes[sent..].ptr, bytes.len - sent, std.os.linux.MSG.NOSIGNAL, null, 0);
            switch (std.os.linux.errno(rc)) {
                .SUCCESS => {},
                .INTR => continue,
                else => return error.TransportFailed,
            }
            if (rc == 0) return error.TransportFailed;
            sent += @intCast(rc);
        }
    }

    fn shutdown(ctx: *anyopaque) void {
        const self: *TcpLink = @ptrCast(@alignCast(ctx));
        // Raw: a nudge races the owner's close by design, and an already-
        // closed socket's EBADF is nothing to report.
        _ = std.os.linux.shutdown(self.stream.socket.handle, std.os.linux.SHUT.RDWR);
    }

    fn close(ctx: *anyopaque) void {
        const self: *TcpLink = @ptrCast(@alignCast(ctx));
        self.stream.close(self.io);
    }
};

/// Per-connection buffers: the send frame (header + the control cap + the
/// MAC) and the handler scratch at the cap — VIRTUAL reservations the
/// allocator mmaps and the kernel commits only as pages are touched, so a
/// connection that never ships a token page never pays for one; the
/// receive buffer grows a chunk at a time as bytes arrive (the lying-
/// length law).
pub const SEND_BUF_LEN: usize = frame.HEADER_LEN + frame.CONTROL_CAP + frame.MAC_BYTES;

pub const Session = struct {
    gpa: std.mem.Allocator,
    framer: Framer,
    link: Link,
    recv: std.ArrayList(u8) = .empty,
    send_buf: []u8,
    scratch: []u8,
    /// The listener's connection id / the dialer's session number (logs).
    id: u64,

    pub fn init(gpa: std.mem.Allocator, link: Link, role: Role, id: u64) std.mem.Allocator.Error!Session {
        const send_buf = try gpa.alloc(u8, SEND_BUF_LEN);
        errdefer gpa.free(send_buf);
        const scratch = try gpa.alloc(u8, frame.CONTROL_CAP);
        errdefer gpa.free(scratch);
        var s: Session = .{ .gpa = gpa, .framer = .init(role), .link = link, .send_buf = send_buf, .scratch = scratch, .id = id };
        try s.recv.ensureTotalCapacityPrecise(gpa, frame.FRAME_CHUNK_BYTES);
        return s;
    }

    pub fn deinit(self: *Session) void {
        self.recv.deinit(self.gpa);
        self.gpa.free(self.send_buf);
        self.gpa.free(self.scratch);
    }

    /// The reply-payload window the service writes into (zero-copy: the
    /// payload is sealed where it was built).
    pub fn payloadWindow(self: *Session) []u8 {
        return self.send_buf[frame.HEADER_LEN..][0..frame.CONTROL_CAP];
    }

    /// Seal and write the frame whose payload the caller built in
    /// `payloadWindow()[0..payload_len]`.
    pub fn sendSealed(self: *Session, h: frame.Header, payload_len: usize) FrameError!void {
        const n = try self.framer.seal(h, self.send_buf, payload_len);
        try self.link.writeAll(self.send_buf[0..n]);
    }

    /// Encode a typed call into the window and send it (one `write_all`
    /// per frame). Returns the correlation the caller passed, for symmetry.
    pub fn sendCall(self: *Session, req: verbs.Request, correlation: u64) FrameError!void {
        var b: codec.Builder = .init(self.payloadWindow());
        verbs.encodeRequest(req, &b) catch return error.Overflow;
        const h = self.framer.nextHeader(@as(verbs.Verb, req), .ok, correlation, false);
        try self.sendSealed(h, b.pos);
    }

    pub fn sendHandshake(self: *Session, comptime P: type, verb: verbs.Verb, payload: P) FrameError!void {
        var b: codec.Builder = .init(self.payloadWindow());
        codec.encodePayload(P, payload, &b) catch return error.Overflow;
        try self.sendSealed(self.framer.handshakeHeader(verb), b.pos);
    }

    /// The receive discipline (module doc). The returned payload lives in
    /// the session's receive buffer until the next `recvFrame`.
    pub fn recvFrame(self: *Session, header_timeout_ms: ?u32) FrameError!Incoming {
        var hb: [frame.HEADER_LEN]u8 = undefined;
        try self.link.readExact(&hb, header_timeout_ms);
        const verb_word = std.mem.readInt(u16, hb[12..14], .little);
        const h = switch (frame.decodeHeader(&hb, Framer.capFor(verb_word))) {
            .ok => |h| h,
            .refused => |r| {
                stats.cluster_wire_header_refusals.inc();
                log.warn("cluster wire: session {d}: frame header refused ({t}) — closing", .{ self.id, r });
                return error.HeaderRefused;
            },
        };
        const mac_len: usize = if (h.macPresent()) frame.MAC_BYTES else 0;
        const body_len = @as(usize, h.len) + mac_len;
        self.recv.clearRetainingCapacity();
        var got: usize = 0;
        while (got < body_len) {
            const round = @min(body_len - got, frame.FRAME_CHUNK_BYTES);
            self.recv.ensureTotalCapacityPrecise(self.gpa, got + round) catch return error.TransportFailed;
            const dst = self.recv.allocatedSlice()[got .. got + round];
            self.link.readExact(dst, FRAME_BODY_TIMEOUT_MS) catch |e| {
                if (e == error.Timeout) stats.cluster_wire_body_timeouts.inc();
                return e;
            };
            got += round;
            self.recv.items.len = got;
        }
        const payload = self.recv.items[0..h.len];
        const mac: ?*const [frame.MAC_BYTES]u8 = if (h.macPresent()) self.recv.items[h.len..][0..frame.MAC_BYTES] else null;
        try self.framer.verify(&hb, h, payload, mac);
        return .{ .header = h, .payload = payload };
    }

    /// The receive buffer's capacity (the lying-length test's instrument).
    pub fn recvCapacity(self: *const Session) usize {
        return self.recv.capacity;
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "two framers back to back: a call and its reply ride (dir, seq) and the MAC; a replayed frame, a reflected frame and a frame from the wrong half are refused; the handshake frames are MAC-less until admission" {
    var coord: Framer = .init(.coordinator);
    var peer: Framer = .init(.peer);
    var buf: [512]u8 = undefined;
    // Handshake frames pass unauthenticated, in either direction.
    var hb: codec.Builder = .init(buf[frame.HEADER_LEN..]);
    try hb.put(verbs.Admitted, .{ .schema = 6, .reserved = 0 });
    const hn = try coord.seal(coord.handshakeHeader(.admitted), &buf, hb.pos);
    const hin = try peer.accept(buf[0..hn]);
    try testing.expectEqual(@as(u16, 0xFFF2), hin.header.verb);
    // A session verb before admission is refused.
    var b0: codec.Builder = .init(buf[frame.HEADER_LEN..]);
    try b0.put(verbs.Empty, .{});
    var pre: frame.Header = coord.handshakeHeader(.admitted);
    pre.verb = @backingInt(verbs.Verb.ping);
    pre.schema = 6;
    const pn = try coord.seal(pre, &buf, 0);
    try testing.expectError(error.SequenceBroken, peer.accept(buf[0..pn]));
    const key: frame.SessionKey = @splat(0x42);
    coord.admit(key);
    peer.admit(key);
    // Call peer → coord.
    var b: codec.Builder = .init(buf[frame.HEADER_LEN..]);
    try verbs.encodeRequest(.{ .resolve_slot = .{ .volume = 0, .slot = 3, .reserved = 0 } }, &b);
    const call_h = peer.nextHeader(.resolve_slot, .ok, 77, false);
    const n = try peer.seal(call_h, &buf, b.pos);
    var saved: [512]u8 = undefined;
    @memcpy(saved[0..n], buf[0..n]);
    const in = try coord.accept(buf[0..n]);
    try testing.expectEqual(@as(u64, 77), in.header.correlation);
    try testing.expectEqual(@as(u64, 0), in.header.seq);
    const req = try verbs.decodeRequest(.resolve_slot, in.payload);
    try testing.expectEqual(@as(u16, 3), req.resolve_slot.slot);
    // Replay: the same bytes again — the seq word moved on.
    try testing.expectError(error.SequenceBroken, coord.accept(saved[0..n]));
    // Reflection: the peer's own frame handed back to it.
    try testing.expectError(error.DirectionWrong, peer.accept(saved[0..n]));
    // A frame from a session with another key.
    var other: Framer = .init(.peer);
    other.admit(@splat(0x43));
    var ob: codec.Builder = .init(buf[frame.HEADER_LEN..]);
    try ob.put(verbs.Empty, .{});
    const on = try other.seal(other.nextHeader(.ping, .ok, 1, false), &buf, 0);
    stats.cluster_wire_mac_failures.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.MacFailed, coord.accept(buf[0..on]));
    try log.testing.expectAllFired();
    // Reply coord → peer.
    var rb: codec.Builder = .init(buf[frame.HEADER_LEN..]);
    try verbs.encodeReply(.{ .resolve_slot = .{ .kind = 1, .reserved = @splat(0), .appender_id = 0, .g = 2, .reserved2 = 0 } }, &rb);
    const rn = try coord.seal(coord.nextHeader(.resolve_slot, .ok, 77, true), &buf, rb.pos);
    const rin = try peer.accept(buf[0..rn]);
    try testing.expect(rin.header.isReply());
    try testing.expectEqual(@as(u64, 77), rin.header.correlation);
    // A session frame stripped of its MAC flag is refused by the MAC law.
    var stripped: [512]u8 = undefined;
    var sb: codec.Builder = .init(stripped[frame.HEADER_LEN..]);
    try sb.put(verbs.Empty, .{});
    var sh = peer.nextHeader(.ping, .ok, 2, false);
    sh.flags &= ~frame.FLAG_MAC_PRESENT;
    const sn = try peer.seal(sh, &stripped, 0);
    stats.cluster_wire_mac_failures.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.MacFailed, coord.accept(stripped[0..sn]));
    try log.testing.expectAllFired();
    // The cap by class: a handshake verb's header is screened at 8 KiB.
    try testing.expectEqual(frame.HANDSHAKE_CAP, Framer.capFor(0xFFF1));
    try testing.expectEqual(frame.CONTROL_CAP, Framer.capFor(0x0500));
    try testing.expectEqual(frame.CONTROL_CAP, Framer.capFor(0x7777));
}

/// A `Link` over two in-memory byte queues (the session tests' pipe; the
/// TCP link is exercised by the listener/dialer e2e on Linux).
pub const PipeLink = struct {
    rx: *std.ArrayList(u8),
    tx: *std.ArrayList(u8),
    gpa: std.mem.Allocator,
    rx_pos: usize = 0,
    /// After this many bytes the "peer" stalls: a read past it times out.
    stall_after: ?usize = null,

    pub fn link(self: *PipeLink) Link {
        return .{ .ctx = self, .vtable = &vtable };
    }

    const vtable: Link.VTable = .{ .readExact = readExact, .writeAll = writeAll, .shutdown = shutdown, .close = close };

    fn readExact(ctx: *anyopaque, buf: []u8, _: ?u32) FrameError!void {
        const self: *PipeLink = @ptrCast(@alignCast(ctx));
        const avail = self.rx.items.len - self.rx_pos;
        if (self.stall_after) |limit| if (self.rx_pos + buf.len > limit) return error.Timeout;
        if (avail < buf.len) return error.Eof;
        @memcpy(buf, self.rx.items[self.rx_pos..][0..buf.len]);
        self.rx_pos += buf.len;
    }
    fn writeAll(ctx: *anyopaque, bytes: []const u8) FrameError!void {
        const self: *PipeLink = @ptrCast(@alignCast(ctx));
        self.tx.appendSlice(self.gpa, bytes) catch return error.TransportFailed;
    }
    fn shutdown(_: *anyopaque) void {}
    fn close(_: *anyopaque) void {}
};

test "a session over a pipe: the frame is sealed in place and read back in chunks; a lying length allocates one chunk, not the cap; a stalled body hits the deadline" {
    const gpa = testing.allocator;
    var a_to_b: std.ArrayList(u8) = .empty;
    defer a_to_b.deinit(gpa);
    var b_to_a: std.ArrayList(u8) = .empty;
    defer b_to_a.deinit(gpa);
    var la: PipeLink = .{ .rx = &b_to_a, .tx = &a_to_b, .gpa = gpa };
    var lb: PipeLink = .{ .rx = &a_to_b, .tx = &b_to_a, .gpa = gpa };
    var a = try Session.init(gpa, la.link(), .peer, 1);
    defer a.deinit();
    var b = try Session.init(gpa, lb.link(), .coordinator, 2);
    defer b.deinit();
    const key: frame.SessionKey = @splat(1);
    a.framer.admit(key);
    b.framer.admit(key);
    try a.sendCall(.{ .resolve_slot = .{ .volume = 0, .slot = 11, .reserved = 0 } }, 5);
    const in = try b.recvFrame(null);
    try testing.expectEqual(@as(u64, 5), in.header.correlation);
    try testing.expectEqual(@as(u16, 11), (try verbs.decodeRequest(.resolve_slot, in.payload)).resolve_slot.slot);
    // A 200 KiB payload arrives in four rounds; the buffer grew to it.
    var big: codec.Builder = .init(a.payloadWindow());
    const objs: [25_000]u64 = @splat(7);
    try verbs.encodeRequest(.{ .token_release = .{ .head = .{ .volume = 0, .n = 25_000, .reserved = 0, .client_id = 1 }, .items = &objs } }, &big);
    try a.sendSealed(a.framer.nextHeader(.token_release, .ok, 6, false), big.pos);
    const in2 = try b.recvFrame(null);
    try testing.expectEqual(@as(u32, 200_016), in2.header.len);
    try testing.expect(b.recvCapacity() >= 200_016 + frame.MAC_BYTES);
    try testing.expect(b.recvCapacity() < frame.CONTROL_CAP);
    // A lying length: a header claiming the cap with nothing behind it.
    var liar = try Session.init(gpa, lb.link(), .coordinator, 3);
    defer liar.deinit();
    liar.framer.admit(key);
    var hb: [frame.HEADER_LEN]u8 = undefined;
    frame.encodeHeader(.{ .flags = frame.FLAG_MAC_PRESENT | frame.FLAG_DIR_PEER_TO_COORD, .len = frame.CONTROL_CAP, .verb = 0x0500, .status = 0, .correlation = 0, .seq = 0, .schema = 1 }, "", &hb);
    std.mem.writeInt(u32, hb[8..12], frame.CONTROL_CAP, .little);
    a_to_b.clearRetainingCapacity();
    lb.rx_pos = 0;
    try a_to_b.appendSlice(gpa, &hb);
    const ten_zero: [10]u8 = @splat(0);
    try a_to_b.appendSlice(gpa, &ten_zero);
    lb.stall_after = frame.HEADER_LEN + 10;
    const before = stats.cluster_wire_body_timeouts.load();
    try testing.expectError(error.Timeout, liar.recvFrame(null));
    try testing.expectEqual(before + 1, stats.cluster_wire_body_timeouts.load());
    // Peak allocation: ONE chunk, never the claimed cap.
    try testing.expect(liar.recvCapacity() <= frame.FRAME_CHUNK_BYTES);
    // A header over the handshake cap for a handshake verb is refused before any read.
    frame.encodeHeader(.{ .flags = frame.FLAG_DIR_PEER_TO_COORD, .len = frame.HANDSHAKE_CAP + 1, .verb = 0xFFF1, .status = 0, .correlation = 0, .seq = 0, .schema = 6 }, "", &hb);
    std.mem.writeInt(u32, hb[8..12], frame.HANDSHAKE_CAP + 1, .little);
    a_to_b.clearRetainingCapacity();
    lb.rx_pos = 0;
    lb.stall_after = null;
    try a_to_b.appendSlice(gpa, &hb);
    log.testing.expectErrorLogs(0);
    try testing.expectError(error.HeaderRefused, liar.recvFrame(null));
    // EOF at a frame boundary.
    try testing.expectError(error.Eof, liar.recvFrame(null));
}
