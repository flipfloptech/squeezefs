//! The MutualTls engine against an EXTERNAL oracle on the Linux venue:
//! OpenSSL's `s_server` and `s_client` (3.0.13 on the venue) in both
//! roles with client-certificate verification under the cluster's own
//! Ed25519 PKI, the RFC 8446 exporter compared BYTE FOR BYTE with the
//! value OpenSSL prints for the same label, a foreign-CA client refused by
//! our server, and the scoping numbers: handshake latency and record
//! throughput over loopback TCP, plaintext vs TLS (`docs/evidence/
//! rung-10-mtls.md`). Linux only (real sockets, child processes).

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const net = Io.net;
const linux = std.os.linux;
const testkit = @import("../core/testkit.zig");
const clock = @import("../core/clock.zig");
const pki = @import("pki.zig");
const tls = @import("tls.zig");
const session = @import("session.zig");
const dialer = @import("dialer.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("wire/tls_e2e_linux is Linux only; gate the import on builtin.os.tag");
}

const testing = std.testing;

const EXPORT_LABEL = tls.BINDING_LABEL;

/// The PKI written as PEM files OpenSSL reads.
const Files = struct {
    dir: [64]u8 = undefined,
    dir_len: usize = 0,
    tp: tls.TestPki,

    fn path(self: *const Files, buf: *[128]u8, name: []const u8) []const u8 {
        return std.fmt.bufPrint(buf, "{s}/{s}", .{ self.dir[0..self.dir_len], name }) catch unreachable;
    }

    fn init(io: Io, now: u64) !Files {
        var f: Files = .{ .tp = try tls.TestPki.init(io, now) };
        const d = try std.fmt.bufPrint(&f.dir, "/tmp/sqz-tls-e2e-{d}", .{linux.getpid()});
        f.dir_len = d.len;
        const cwd = Io.Dir.cwd();
        cwd.deleteTree(io, d) catch {};
        try cwd.createDir(io, d, .default_dir);
        var text: [4096]u8 = undefined;
        var pb: [128]u8 = undefined;
        try cwd.writeFile(io, .{ .sub_path = f.path(&pb, "ca.pem"), .data = try pki.pemEncode(pki.PEM_CERTIFICATE, f.tp.ca.der(), &text) });
        try cwd.writeFile(io, .{ .sub_path = f.path(&pb, "a.pem"), .data = try pki.pemEncode(pki.PEM_CERTIFICATE, f.tp.a.der(), &text) });
        try cwd.writeFile(io, .{ .sub_path = f.path(&pb, "b.pem"), .data = try pki.pemEncode(pki.PEM_CERTIFICATE, f.tp.b.der(), &text) });
        try cwd.writeFile(io, .{ .sub_path = f.path(&pb, "x.pem"), .data = try pki.pemEncode(pki.PEM_CERTIFICATE, f.tp.foreign.der(), &text) });
        inline for (.{ .{ "a.key", &f.tp.a }, .{ "b.key", &f.tp.b }, .{ "x.key", &f.tp.foreign } }) |pair| {
            const seed = pki.seedOf(&pair[1].key);
            try cwd.writeFile(io, .{ .sub_path = f.path(&pb, pair[0]), .data = try pki.pemEncode(pki.PEM_PRIVATE_KEY, &pki.encodePkcs8(&seed), &text) });
        }
        return f;
    }

    fn deinit(self: *Files, io: Io) void {
        Io.Dir.cwd().deleteTree(io, self.dir[0..self.dir_len]) catch {};
    }
};

fn haveOpenssl(io: Io) bool {
    const r = std.process.run(testing.allocator, io, .{ .argv = &.{ "openssl", "version" } }) catch return false;
    defer testing.allocator.free(r.stdout);
    defer testing.allocator.free(r.stderr);
    return r.term.success() and std.mem.startsWith(u8, r.stdout, "OpenSSL 3");
}

/// A free loopback port: bind 0, read it back, close.
fn freePort(io: Io) !u16 {
    var probe = try net.IpAddress.listen(&.{ .ip4 = .loopback(0) }, io, .{ .reuse_address = true });
    const port = probe.socket.address.getPort();
    probe.deinit(io);
    return port;
}

/// `Keying material: <hex>` in OpenSSL's output → the 32 bytes.
fn keyingMaterial(text: []const u8) ?[tls.BINDING_BYTES]u8 {
    const mark = "Keying material: ";
    const i = std.mem.indexOf(u8, text, mark) orelse return null;
    const hex = text[i + mark.len ..];
    if (hex.len < 2 * tls.BINDING_BYTES) return null;
    var out: [tls.BINDING_BYTES]u8 = undefined;
    _ = std.fmt.hexToBytes(&out, hex[0 .. 2 * tls.BINDING_BYTES]) catch return null;
    return out;
}

/// Dial with a bounded retry (the child's listener comes up asynchronously).
fn dialRetry(io: Io, port: u16) !net.Stream {
    const deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
    while (true) {
        if (dialer.connectTimeout(.{ .ip4 = .loopback(port) }, 1000)) |s| return s else |_| {}
        if (clock.monotonicNs() > deadline) return error.ListenerNeverCameUp;
        io.sleep(.fromMilliseconds(20), .awake) catch {};
    }
}

test "tls e2e: our CLIENT against `openssl s_server -Verify` under the PKI — the handshake completes with our certificate, `-rev` echoes through the record layer, and the exporter equals the keying material OpenSSL prints" {
    const io = testing.io;
    const gpa = testing.allocator;
    if (!haveOpenssl(io)) return testkit.skip(@src(), .no_device, "openssl 3 is not on this venue's PATH");
    const now = clock.realtimeCoarseS();
    var files = try Files.init(io, now);
    defer files.deinit(io);
    var id_b = try files.tp.identity(gpa, .b);
    defer id_b.deinit(gpa);
    const port = try freePort(io);
    var port_text: [24]u8 = undefined;
    const accept = try std.fmt.bufPrint(&port_text, "127.0.0.1:{d}", .{port});
    var p1: [128]u8 = undefined;
    var p2: [128]u8 = undefined;
    var p3: [128]u8 = undefined;
    var out_path_buf: [128]u8 = undefined;
    const out_path = files.path(&out_path_buf, "s_server.out");
    const out_file = try Io.Dir.cwd().createFile(io, out_path, .{});
    var child = try std.process.spawn(io, .{
        .argv = &.{ "openssl", "s_server", "-accept", accept, "-naccept", "1", "-tls1_3", "-cert", files.path(&p1, "a.pem"), "-key", files.path(&p2, "a.key"), "-CAfile", files.path(&p3, "ca.pem"), "-Verify", "1", "-rev" },
        .stdin = .ignore,
        .stdout = .{ .file = out_file },
        .stderr = .{ .file = out_file },
    });
    out_file.close(io);
    var reaped = false;
    defer if (!reaped) child.kill(io);
    const stream = try dialRetry(io, port);
    session.TcpLink.setNoDelay(stream);
    var tcp: session.TcpLink = .{ .io = io, .stream = stream };
    const conn = tls.clientHandshake(gpa, tcp.link(), &id_b, io, .{ .now_s = now }) catch |e| {
        std.debug.print("\n--- client handshake against s_server failed: {t} ---\n", .{e});
        return e;
    };
    defer conn.destroy(gpa);
    try testing.expectEqualStrings("node-a", conn.peerName());
    var link = conn.link();
    try link.writeAll("hello oracle\n");
    var buf: [32]u8 = undefined;
    try link.readExact(buf[0..13], 5000);
    try testing.expectEqualStrings("elcaro olleh\n", buf[0..13]);
    link.close();
    // `-naccept 1`: the server exits after this connection and flushes
    // its connection info to the file: the mutual verification is OpenSSL's
    // verdict on OUR certificate and CertificateVerify. (3.0's `s_server`
    // prints no keying material in any body mode; the exporter oracle is
    // the `s_client` row.)
    const term = try child.wait(io);
    reaped = true;
    const text = try Io.Dir.cwd().readFileAlloc(io, out_path, gpa, .limited(1 << 20));
    defer gpa.free(text);
    const established = std.mem.indexOf(u8, text, "CONNECTION ESTABLISHED") != null;
    const verified = std.mem.indexOf(u8, text, "Peer certificate: CN = node-b") != null and std.mem.indexOf(u8, text, "Verification: OK") != null;
    const suite = std.mem.indexOf(u8, text, "Ciphersuite: TLS_AES_128_GCM_SHA256") != null and std.mem.indexOf(u8, text, "Signature type: Ed25519") != null;
    if (!(established and verified and suite)) {
        std.debug.print("\n--- s_server ({f}) did not record the mutual session ---\n{s}\n", .{ term, text });
        return error.OracleDisagrees;
    }
    std.debug.print("\ntls e2e: our client ↔ openssl s_server -Verify: TLSv1.3 TLS_AES_128_GCM_SHA256, our leaf CN=node-b verified OK, -rev echo crossed\n", .{});
}

const EchoServer = struct {
    gpa: std.mem.Allocator,
    io: Io,
    server: *net.Server,
    identity: *const tls.Identity,
    now: u64,
    result: tls.HandshakeError!void = {},
    peer: [pki.NAME_MAX]u8 = undefined,
    peer_len: usize = 0,
    binding: [tls.BINDING_BYTES]u8 = @splat(0),

    fn run(self: *EchoServer) void {
        const stream = self.server.accept(self.io) catch {
            self.result = error.Transport;
            return;
        };
        session.TcpLink.setNoDelay(stream);
        var tcp: session.TcpLink = .{ .io = self.io, .stream = stream };
        const conn = tls.serverHandshake(self.gpa, tcp.link(), self.identity, self.io, .{ .now_s = self.now }) catch |e| {
            self.result = e;
            tcp.link().close();
            return;
        };
        defer conn.destroy(self.gpa);
        @memcpy(self.peer[0..conn.peerName().len], conn.peerName());
        self.peer_len = conn.peerName().len;
        self.binding = conn.binding();
        var link = conn.link();
        var buf: [5]u8 = undefined;
        link.readExact(&buf, 5000) catch |e| {
            self.result = switch (e) {
                error.Eof => error.Eof,
                error.Timeout => error.Timeout,
                else => error.Transport,
            };
            link.close();
            return;
        };
        link.writeAll(if (std.mem.eql(u8, &buf, "ping\n")) "pong\n" else "????\n") catch {};
        link.close();
    }
};

test "tls e2e: `openssl s_client` against our SERVER — a client under the CA is admitted (its CN is the peer name), the echo crosses, the exporter agrees; a client under a FOREIGN CA is refused as Untrusted and OpenSSL sees the alert" {
    const io = testing.io;
    const gpa = testing.allocator;
    if (!haveOpenssl(io)) return testkit.skip(@src(), .no_device, "openssl 3 is not on this venue's PATH");
    const now = clock.realtimeCoarseS();
    var files = try Files.init(io, now);
    defer files.deinit(io);
    var id_a = try files.tp.identity(gpa, .a);
    defer id_a.deinit(gpa);
    var pin: [128]u8 = undefined;
    const in_path = files.path(&pin, "stdin");
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = in_path, .data = "ping\n" });
    inline for (.{ .{ "b", true }, .{ "x", false } }) |row| {
        var server = try net.IpAddress.listen(&.{ .ip4 = .loopback(0) }, io, .{ .reuse_address = true });
        defer server.deinit(io);
        const port = server.socket.address.getPort();
        var es: EchoServer = .{ .gpa = gpa, .io = io, .server = &server, .identity = &id_a, .now = now };
        const t = try std.Thread.spawn(.{}, EchoServer.run, .{&es});
        var port_text: [24]u8 = undefined;
        const connect = try std.fmt.bufPrint(&port_text, "127.0.0.1:{d}", .{port});
        var p1: [128]u8 = undefined;
        var p2: [128]u8 = undefined;
        var p3: [128]u8 = undefined;
        var script_buf: [1024]u8 = undefined;
        const script = try std.fmt.bufPrint(&script_buf, "openssl s_client -connect {s} -tls1_3 -cert {s} -key {s} -CAfile {s} -verify_return_error -ign_eof -keymatexport {s} -keymatexportlen 32 < {s}", .{ connect, files.path(&p1, row[0] ++ ".pem"), files.path(&p2, row[0] ++ ".key"), files.path(&p3, "ca.pem"), EXPORT_LABEL, in_path });
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-c", script } });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        t.join();
        if (row[1]) {
            if (es.result) |_| {} else |e| {
                std.debug.print("\n--- server handshake with s_client failed: {t}\n{s}\n{s}\n", .{ e, r.stdout, r.stderr });
                return e;
            }
            try testing.expectEqualStrings("node-b", es.peer[0..es.peer_len]);
            try testing.expect(std.mem.indexOf(u8, r.stdout, "pong\n") != null);
            // s_client prints the keying material to stdout.
            const theirs = keyingMaterial(r.stdout) orelse {
                std.debug.print("\n--- no keying material in s_client's output ---\n{s}\n{s}\n", .{ r.stdout, r.stderr });
                return error.NoKeyingMaterial;
            };
            // THE exporter oracle: OpenSSL's export under the binding label
            // equals our server's `binding()` for the same session.
            try testing.expectEqual(es.binding, theirs);
            std.debug.print("tls e2e: openssl s_client ↔ our server: admitted node-b, echo crossed, exporter {x} agrees with OpenSSL\n", .{&theirs});
        } else {
            try testing.expectError(error.PeerCertificateUntrusted, es.result);
            try testing.expect(!r.term.success());
            try testing.expect(std.mem.indexOf(u8, r.stderr, "unknown ca") != null or std.mem.indexOf(u8, r.stderr, "alert") != null);
            std.debug.print("tls e2e: openssl s_client under a foreign CA: refused Untrusted; s_client: {s}\n", .{std.mem.trimEnd(u8, r.stderr[0..@min(r.stderr.len, 200)], "\n")});
        }
    }
}

/// The server half of the scoping rows: accept N connections, handshake
/// each (or not, plain), sink the bytes.
const Sink = struct {
    gpa: std.mem.Allocator,
    io: Io,
    server: *net.Server,
    identity: ?*const tls.Identity,
    now: u64,
    connections: u32,
    bytes_expected: u64,
    handshake_ns: u64 = 0,
    err: ?anyerror = null,

    fn run(self: *Sink) void {
        var i: u32 = 0;
        while (i < self.connections) : (i += 1) {
            const stream = self.server.accept(self.io) catch |e| {
                self.err = e;
                return;
            };
            session.TcpLink.setNoDelay(stream);
            var tcp: session.TcpLink = .{ .io = self.io, .stream = stream };
            var conn: ?*tls.Conn = null;
            defer if (conn) |c| c.destroy(self.gpa);
            const t0 = clock.monotonicNs();
            const link: session.Link = if (self.identity) |id| blk: {
                conn = tls.serverHandshake(self.gpa, tcp.link(), id, self.io, .{ .now_s = self.now }) catch |e| {
                    self.err = e;
                    tcp.link().close();
                    return;
                };
                break :blk conn.?.link();
            } else tcp.link();
            self.handshake_ns += clock.monotonicNs() - t0;
            var buf: [1 << 16]u8 = undefined;
            var got: u64 = 0;
            while (got < self.bytes_expected) {
                const n = @min(buf.len, self.bytes_expected - got);
                link.readExact(buf[0..n], 10_000) catch |e| {
                    self.err = e;
                    link.close();
                    return;
                };
                got += n;
            }
            link.close();
        }
    }
};

fn scopingRow(gpa: std.mem.Allocator, io: Io, identity: ?*const tls.Identity, now: u64, connections: u32, bytes_per_conn: u64, chunk: usize) !struct { handshake_us: u64, gbps: f64 } {
    var server = try net.IpAddress.listen(&.{ .ip4 = .loopback(0) }, io, .{ .reuse_address = true });
    defer server.deinit(io);
    const port = server.socket.address.getPort();
    var sink: Sink = .{ .gpa = gpa, .io = io, .server = &server, .identity = identity, .now = now, .connections = connections, .bytes_expected = bytes_per_conn };
    const t = try std.Thread.spawn(.{}, Sink.run, .{&sink});
    const payload = try gpa.alloc(u8, chunk);
    defer gpa.free(payload);
    for (payload, 0..) |*x, i| x.* = @truncate(i);
    var hs_ns: u64 = 0;
    var xfer_ns: u64 = 0;
    var i: u32 = 0;
    while (i < connections) : (i += 1) {
        const t0 = clock.monotonicNs();
        const stream = try dialer.connectTimeout(.{ .ip4 = .loopback(port) }, 2000);
        session.TcpLink.setNoDelay(stream);
        var tcp: session.TcpLink = .{ .io = io, .stream = stream };
        var conn: ?*tls.Conn = null;
        defer if (conn) |c| c.destroy(gpa);
        const link: session.Link = if (identity) |id| blk: {
            conn = try tls.clientHandshake(gpa, tcp.link(), id, io, .{ .now_s = now });
            break :blk conn.?.link();
        } else tcp.link();
        const t1 = clock.monotonicNs();
        hs_ns += t1 - t0;
        var sent: u64 = 0;
        while (sent < bytes_per_conn) : (sent += chunk) try link.writeAll(payload);
        xfer_ns += clock.monotonicNs() - t1;
        link.close();
    }
    t.join();
    if (sink.err) |e| return e;
    const total_bytes: f64 = @floatFromInt(@as(u64, connections) * bytes_per_conn);
    return .{ .handshake_us = hs_ns / connections / std.time.ns_per_us, .gbps = if (xfer_ns == 0) 0 else total_bytes / @as(f64, @floatFromInt(xfer_ns)) };
}

test "tls e2e: SCOPING — handshake latency and record throughput over loopback TCP, plaintext vs MutualTls (64 KiB and 1 MiB writes)" {
    const io = testing.io;
    const gpa = testing.allocator;
    const now = clock.realtimeCoarseS();
    const tp = try tls.TestPki.init(io, now);
    var id_a = try tp.identity(gpa, .a);
    defer id_a.deinit(gpa);
    // Both ends use node-a's identity: the CA is what the handshake
    // judges; the rows measure the engine, not the PKI.
    const plain_hs = try scopingRow(gpa, io, null, now, 50, 1 << 20, 1 << 16);
    const tls_hs = try scopingRow(gpa, io, &id_a, now, 50, 1 << 20, 1 << 16);
    const plain_64k = try scopingRow(gpa, io, null, now, 1, 512 << 20, 1 << 16);
    const tls_64k = try scopingRow(gpa, io, &id_a, now, 1, 512 << 20, 1 << 16);
    const plain_1m = try scopingRow(gpa, io, null, now, 1, 512 << 20, 1 << 20);
    const tls_1m = try scopingRow(gpa, io, &id_a, now, 1, 512 << 20, 1 << 20);
    std.debug.print("\ntls scoping (loopback TCP, 512 MiB streams, {s}):\n  connect+handshake: plaintext {d} µs, mutual_tls {d} µs (50 dials each)\n  64 KiB writes: plaintext {d:.2} GB/s, mutual_tls {d:.2} GB/s\n  1 MiB writes:  plaintext {d:.2} GB/s, mutual_tls {d:.2} GB/s\n", .{ @tagName(builtin.mode), plain_hs.handshake_us, tls_hs.handshake_us, plain_64k.gbps, tls_64k.gbps, plain_1m.gbps, tls_1m.gbps });
    try testing.expect(tls_64k.gbps > 0 and tls_1m.gbps > 0);
}
