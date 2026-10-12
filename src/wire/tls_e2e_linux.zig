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

/// The kernel's posture, once per process (the probe is a loopback pair).
fn kernelTls() tls.ktls.Probe {
    return tls.ktls.probeKernel();
}

/// The skip every kTLS row returns on a kernel without the ULP: ledgered
/// `other_step` naming the kernel (the orchestrator's rule — the row goes
/// red→green the moment the kernel carries CONFIG_TLS).
fn skipNoKtls(src: std.builtin.SourceLocation) testkit.Error {
    var rel: [64]u8 = undefined;
    const uts = std.posix.uname();
    const release = std.mem.sliceTo(&uts.release, 0);
    const detail = std.fmt.bufPrint(&rel, "kernel {s}: no tls ULP (CONFIG_TLS)", .{release[0..@min(release.len, 40)]}) catch "kernel without the tls ULP (CONFIG_TLS)";
    return testkit.skip(src, .other_step, detail);
}

/// The record-layer options for a row: the userspace layer, or the kernel's
/// over `fd`.
fn layerOpts(layer: tls.RecordLayer, fd: linux.fd_t, now: u64) tls.Options {
    return .{ .now_s = now, .record_layer = layer, .socket = if (layer == .ktls) fd else null };
}

fn clientAgainstSServer(layer: tls.RecordLayer) !void {
    const io = testing.io;
    const gpa = testing.allocator;
    if (!haveOpenssl(io)) return testkit.skip(@src(), .no_device, "openssl 3 is not on this venue's PATH");
    if (layer == .ktls and kernelTls() == .absent) return skipNoKtls(@src());
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
    const tickets0 = stats.cluster_wire_tls_tickets_ignored.load();
    const conn = tls.clientHandshake(gpa, tcp.link(), &id_b, io, layerOpts(layer, stream.socket.handle, now)) catch |e| {
        std.debug.print("\n--- client handshake against s_server failed: {t} ---\n", .{e});
        return e;
    };
    defer conn.destroy(gpa);
    try testing.expectEqualStrings("node-a", conn.peerName());
    try testing.expect(conn.confidential());
    var link = conn.link();
    try link.writeAll("hello oracle\n");
    var buf: [32]u8 = undefined;
    try link.readExact(buf[0..13], 5000);
    try testing.expectEqualStrings("elcaro olleh\n", buf[0..13]);
    // OpenSSL's server sends its NewSessionTickets after the handshake:
    // they arrived as handshake records ahead of the echo, ignored and
    // counted (both record layers).
    try testing.expect(stats.cluster_wire_tls_tickets_ignored.load() > tickets0);
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
    std.debug.print("\ntls e2e [{t}]: our client ↔ openssl s_server -Verify: TLSv1.3 TLS_AES_128_GCM_SHA256, our leaf CN=node-b verified OK, -rev echo crossed, {d} ticket(s) ignored\n", .{ layer, stats.cluster_wire_tls_tickets_ignored.load() - tickets0 });
}

test "tls e2e: our CLIENT against `openssl s_server -Verify` under the PKI — the handshake completes with our certificate, `-rev` echoes through the record layer, OpenSSL's NewSessionTickets are ignored and counted (the userspace record layer)" {
    try clientAgainstSServer(.userspace);
}

test "tls e2e [ktls]: our CLIENT against `openssl s_server -Verify` over KERNEL TLS — the keys handed to TLS_TX/TLS_RX after the handshake, the `-rev` echo through the kernel's records, the tickets dispatched off the TLS_GET_RECORD_TYPE cmsg" {
    try clientAgainstSServer(.ktls);
}

const EchoServer = struct {
    gpa: std.mem.Allocator,
    io: Io,
    server: *net.Server,
    identity: *const tls.Identity,
    now: u64,
    layer: tls.RecordLayer = .userspace,
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
        const conn = tls.serverHandshake(self.gpa, tcp.link(), self.identity, self.io, layerOpts(self.layer, stream.socket.handle, self.now)) catch |e| {
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

fn sClientAgainstOurServer(layer: tls.RecordLayer) !void {
    const io = testing.io;
    const gpa = testing.allocator;
    if (!haveOpenssl(io)) return testkit.skip(@src(), .no_device, "openssl 3 is not on this venue's PATH");
    if (layer == .ktls and kernelTls() == .absent) return skipNoKtls(@src());
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
        var es: EchoServer = .{ .gpa = gpa, .io = io, .server = &server, .identity = &id_a, .now = now, .layer = layer };
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
            std.debug.print("tls e2e [{t}]: openssl s_client ↔ our server: admitted node-b, echo crossed, exporter {x} agrees with OpenSSL\n", .{ layer, &theirs });
        } else {
            try testing.expectError(error.PeerCertificateUntrusted, es.result);
            try testing.expect(!r.term.success());
            try testing.expect(std.mem.indexOf(u8, r.stderr, "unknown ca") != null or std.mem.indexOf(u8, r.stderr, "alert") != null);
            std.debug.print("tls e2e [{t}]: openssl s_client under a foreign CA: refused Untrusted; s_client: {s}\n", .{ layer, std.mem.trimEnd(u8, r.stderr[0..@min(r.stderr.len, 200)], "\n") });
        }
    }
}

test "tls e2e: `openssl s_client` against our SERVER — a client under the CA is admitted (its CN is the peer name), the echo crosses, the exporter agrees; a client under a FOREIGN CA is refused as Untrusted and OpenSSL sees the alert (the userspace record layer)" {
    try sClientAgainstOurServer(.userspace);
}

test "tls e2e [ktls]: `openssl s_client` against our SERVER over KERNEL TLS — admitted, the echo through the kernel's records, the exporter agrees, close_notify through TLS_SET_RECORD_TYPE; the foreign-CA client refused before any key reaches the kernel" {
    try sClientAgainstOurServer(.ktls);
}

// ---------------------------------------------------------------------------
// The kernel record layer on a loopback pair of OUR engines.
// ---------------------------------------------------------------------------

const KtlsPair = struct {
    server: *tls.Conn,
    client: *tls.Conn,
    server_tcp: session.TcpLink,
    client_tcp: session.TcpLink,

    const Accept = struct {
        gpa: std.mem.Allocator,
        io: Io,
        server: *net.Server,
        identity: *const tls.Identity,
        now: u64,
        tcp: session.TcpLink = undefined,
        conn: ?*tls.Conn = null,
        err: ?anyerror = null,

        fn run(self: *Accept) void {
            const stream = self.server.accept(self.io) catch |e| {
                self.err = e;
                return;
            };
            session.TcpLink.setNoDelay(stream);
            self.tcp = .{ .io = self.io, .stream = stream };
            self.conn = tls.serverHandshake(self.gpa, self.tcp.link(), self.identity, self.io, layerOpts(.ktls, stream.socket.handle, self.now)) catch |e| {
                self.err = e;
                self.tcp.link().close();
                return;
            };
        }
    };

    fn init(gpa: std.mem.Allocator, io: Io, server_id: *const tls.Identity, client_id: *const tls.Identity, now: u64) !KtlsPair {
        var server = try net.IpAddress.listen(&.{ .ip4 = .loopback(0) }, io, .{ .reuse_address = true });
        defer server.deinit(io);
        const port = server.socket.address.getPort();
        var acc: Accept = .{ .gpa = gpa, .io = io, .server = &server, .identity = server_id, .now = now };
        const t = try std.Thread.spawn(.{}, Accept.run, .{&acc});
        const stream = try dialer.connectTimeout(.{ .ip4 = .loopback(port) }, 2000);
        session.TcpLink.setNoDelay(stream);
        var tcp: session.TcpLink = .{ .io = io, .stream = stream };
        const client = tls.clientHandshake(gpa, tcp.link(), client_id, io, layerOpts(.ktls, stream.socket.handle, now)) catch |e| {
            tcp.link().close();
            t.join();
            if (acc.conn) |c| c.destroy(gpa);
            return e;
        };
        t.join();
        if (acc.err) |e| {
            client.destroy(gpa);
            tcp.link().close();
            return e;
        }
        return .{ .server = acc.conn.?, .client = client, .server_tcp = acc.tcp, .client_tcp = tcp };
    }

    /// After placement: the Conns' inner links point at THIS pair's
    /// `TcpLink`s (the handshake ran over locals of `init`).
    fn arm(self: *KtlsPair) void {
        self.server.inner = self.server_tcp.link();
        self.client.inner = self.client_tcp.link();
    }

    fn deinit(self: *KtlsPair, gpa: std.mem.Allocator) void {
        self.server.destroy(gpa);
        self.client.destroy(gpa);
    }
};

test "tls e2e [ktls]: a loopback pair of our engines over KERNEL TLS — confidential() only once both halves are installed; records both ways through the kernel; a 1 MiB + 80 B frame; our KeyUpdate (TLS_SET_RECORD_TYPE then a second TLS_TX) re-keys the peer's RX by a second TLS_RX and the stream continues; a peer-requested update is honoured by the next write; close_notify is an orderly EOF, counted; a fatal alert closes loud and counted" {
    const io = testing.io;
    const gpa = testing.allocator;
    if (kernelTls() == .absent) return skipNoKtls(@src());
    const now = clock.realtimeCoarseS();
    const tp = try tls.TestPki.init(io, now);
    var id_a = try tp.identity(gpa, .a);
    defer id_a.deinit(gpa);
    var id_b = try tp.identity(gpa, .b);
    defer id_b.deinit(gpa);
    {
        var p = try KtlsPair.init(gpa, io, &id_a, &id_b, now);
        p.arm();
        defer p.deinit(gpa);
        try testing.expect(p.server.ktls_tx and p.server.ktls_rx and p.client.ktls_tx and p.client.ktls_rx);
        try testing.expect(p.server.confidential() and p.client.confidential());
        try testing.expectEqual(tls.RecordLayer.ktls, p.server.record_layer);
        var sl = p.server.link();
        var cl = p.client.link();
        try cl.writeAll("hello through the kernel");
        var buf: [64]u8 = undefined;
        try sl.readExact(buf[0..24], 2000);
        try testing.expectEqualStrings("hello through the kernel", buf[0..24]);
        try sl.writeAll("and back");
        try cl.readExact(buf[0..8], 2000);
        try testing.expectEqualStrings("and back", buf[0..8]);
        // A frame wider than a record.
        const big = try gpa.alloc(u8, (1 << 20) + 80);
        defer gpa.free(big);
        for (big, 0..) |*x, i| x.* = @truncate(i *% 31);
        try cl.writeAll(big);
        // One small record before it, then ⌈(1 MiB + 80) / 2^14⌉ = 65.
        try testing.expectEqual(@as(u64, 1 + 65), p.client.ktls_records_sent);
        const got = try gpa.alloc(u8, big.len);
        defer gpa.free(got);
        try sl.readExact(got, 5000);
        try testing.expectEqualSlices(u8, big, got);
        // Our KeyUpdate from the server side (forced through the ceiling):
        // the client's next read sees the handshake record, re-keys RX.
        const ku0 = stats.cluster_wire_tls_key_updates.load();
        p.server.ktls_records_sent = tls.KEY_UPDATE_RECORDS;
        try sl.writeAll("after the update");
        try testing.expectEqual(ku0 + 1, stats.cluster_wire_tls_key_updates.load());
        try testing.expectEqual(@as(u64, 1), p.server.ktls_records_sent);
        try cl.readExact(buf[0..16], 2000);
        try testing.expectEqualStrings("after the update", buf[0..16]);
        try testing.expectEqual(@as(u32, 1), p.client.key_updates_received);
        // A peer-REQUESTED update: hand-built on the server (the message
        // under the current key, then its own TX re-key), honoured by the
        // client's next write.
        const req = [_]u8{ @backingInt(std.crypto.tls.HandshakeType.key_update), 0, 0, 1, @backingInt(std.crypto.tls.KeyUpdateRequest.update_requested) };
        try tls.ktls.sendRecord(p.server.ktls_fd, @backingInt(std.crypto.tls.ContentType.handshake), &req);
        try tls.ktls.rotateTx(p.server);
        try sl.writeAll("x");
        try cl.readExact(buf[0..1], 2000);
        try testing.expect(p.client.peer_update_requested.load(.acquire));
        try testing.expectEqual(@as(u32, 2), p.client.key_updates_received);
        try cl.writeAll("rotated");
        try testing.expectEqual(@as(u32, 1), p.client.key_updates_sent);
        try sl.readExact(buf[0..7], 2000);
        try testing.expectEqualStrings("rotated", buf[0..7]);
        // close_notify from the client: an orderly EOF at the server, counted.
        const cn0 = stats.cluster_wire_tls_close_notify.load();
        cl.close();
        try testing.expectError(error.Eof, sl.readExact(buf[0..1], 2000));
        try testing.expectEqual(cn0 + 1, stats.cluster_wire_tls_close_notify.load());
        sl.close();
    }
    {
        // A fatal alert (internal_error) from the server: the client's read
        // closes loud and counted.
        var p = try KtlsPair.init(gpa, io, &id_a, &id_b, now);
        p.arm();
        defer p.deinit(gpa);
        const fa0 = stats.cluster_wire_tls_fatal_alerts.load();
        try tls.ktls.sendRecord(p.server.ktls_fd, @backingInt(std.crypto.tls.ContentType.alert), &.{ @backingInt(std.crypto.tls.Alert.Level.fatal), @backingInt(std.crypto.tls.Alert.Description.internal_error) });
        var cl = p.client.link();
        var buf: [8]u8 = undefined;
        try testing.expectError(error.TransportFailed, cl.readExact(buf[0..1], 2000));
        try testing.expectEqual(fa0 + 1, stats.cluster_wire_tls_fatal_alerts.load());
        cl.close();
        p.server.link().close();
    }
}

/// The server half of the scoping rows: accept N connections, handshake
/// each (or not, plain), sink the bytes.
const Sink = struct {
    gpa: std.mem.Allocator,
    io: Io,
    server: *net.Server,
    identity: ?*const tls.Identity,
    layer: tls.RecordLayer = .userspace,
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
                conn = tls.serverHandshake(self.gpa, tcp.link(), id, self.io, layerOpts(self.layer, stream.socket.handle, self.now)) catch |e| {
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

/// The process's CPU time (both ends of the loopback live here), ns.
fn processCpuNs() u64 {
    var ts: linux.timespec = undefined;
    _ = linux.clock_gettime(.PROCESS_CPUTIME_ID, &ts);
    return @as(u64, @intCast(ts.sec)) * std.time.ns_per_s + @as(u64, @intCast(ts.nsec));
}

fn scopingRow(gpa: std.mem.Allocator, io: Io, identity: ?*const tls.Identity, layer: tls.RecordLayer, now: u64, connections: u32, bytes_per_conn: u64, chunk: usize) !struct { handshake_us: u64, gbps: f64, cpu_s_per_gb: f64 } {
    var server = try net.IpAddress.listen(&.{ .ip4 = .loopback(0) }, io, .{ .reuse_address = true });
    defer server.deinit(io);
    const port = server.socket.address.getPort();
    var sink: Sink = .{ .gpa = gpa, .io = io, .server = &server, .identity = identity, .layer = layer, .now = now, .connections = connections, .bytes_expected = bytes_per_conn };
    const t = try std.Thread.spawn(.{}, Sink.run, .{&sink});
    const payload = try gpa.alloc(u8, chunk);
    defer gpa.free(payload);
    for (payload, 0..) |*x, i| x.* = @truncate(i);
    var hs_ns: u64 = 0;
    var xfer_ns: u64 = 0;
    var cpu_ns: u64 = 0;
    var i: u32 = 0;
    while (i < connections) : (i += 1) {
        const t0 = clock.monotonicNs();
        const stream = try dialer.connectTimeout(.{ .ip4 = .loopback(port) }, 2000);
        session.TcpLink.setNoDelay(stream);
        var tcp: session.TcpLink = .{ .io = io, .stream = stream };
        var conn: ?*tls.Conn = null;
        defer if (conn) |c| c.destroy(gpa);
        const link: session.Link = if (identity) |id| blk: {
            conn = try tls.clientHandshake(gpa, tcp.link(), id, io, layerOpts(layer, stream.socket.handle, now));
            break :blk conn.?.link();
        } else tcp.link();
        const t1 = clock.monotonicNs();
        hs_ns += t1 - t0;
        const c0 = processCpuNs();
        var sent: u64 = 0;
        while (sent < bytes_per_conn) : (sent += chunk) try link.writeAll(payload);
        xfer_ns += clock.monotonicNs() - t1;
        cpu_ns += processCpuNs() - c0;
        link.close();
    }
    t.join();
    if (sink.err) |e| return e;
    const total_bytes: f64 = @floatFromInt(@as(u64, connections) * bytes_per_conn);
    const gb = total_bytes / 1e9;
    return .{ .handshake_us = hs_ns / connections / std.time.ns_per_us, .gbps = if (xfer_ns == 0) 0 else total_bytes / @as(f64, @floatFromInt(xfer_ns)), .cpu_s_per_gb = @as(f64, @floatFromInt(cpu_ns)) / 1e9 / gb };
}

test "tls e2e: SCOPING — connect+handshake, record throughput and CPU per GB over loopback TCP: plaintext vs MutualTls on the userspace record layer vs MutualTls on KERNEL TLS (the kTLS column when the kernel carries the ULP; 64 KiB and 1 MiB writes)" {
    const io = testing.io;
    const gpa = testing.allocator;
    const now = clock.realtimeCoarseS();
    const tp = try tls.TestPki.init(io, now);
    var id_a = try tp.identity(gpa, .a);
    defer id_a.deinit(gpa);
    const ktls_present = kernelTls() == .present;
    // Both ends use node-a's identity: the CA is what the handshake
    // judges; the rows measure the engine, not the PKI.
    const plain_hs = try scopingRow(gpa, io, null, .userspace, now, 50, 1 << 20, 1 << 16);
    const tls_hs = try scopingRow(gpa, io, &id_a, .userspace, now, 50, 1 << 20, 1 << 16);
    const plain_64k = try scopingRow(gpa, io, null, .userspace, now, 1, 512 << 20, 1 << 16);
    const tls_64k = try scopingRow(gpa, io, &id_a, .userspace, now, 1, 512 << 20, 1 << 16);
    const plain_1m = try scopingRow(gpa, io, null, .userspace, now, 1, 512 << 20, 1 << 20);
    const tls_1m = try scopingRow(gpa, io, &id_a, .userspace, now, 1, 512 << 20, 1 << 20);
    std.debug.print("\ntls scoping (loopback TCP, 512 MiB streams, {s}; CPU = both ends in this process):\n  connect+handshake: plaintext {d} µs, mutual_tls {d} µs (50 dials each)\n  64 KiB writes: plaintext {d:.2} GB/s {d:.3} CPU-s/GB, mutual_tls/userspace {d:.2} GB/s {d:.3} CPU-s/GB\n  1 MiB writes:  plaintext {d:.2} GB/s {d:.3} CPU-s/GB, mutual_tls/userspace {d:.2} GB/s {d:.3} CPU-s/GB\n", .{ @tagName(builtin.mode), plain_hs.handshake_us, tls_hs.handshake_us, plain_64k.gbps, plain_64k.cpu_s_per_gb, tls_64k.gbps, tls_64k.cpu_s_per_gb, plain_1m.gbps, plain_1m.cpu_s_per_gb, tls_1m.gbps, tls_1m.cpu_s_per_gb });
    try testing.expect(tls_64k.gbps > 0 and tls_1m.gbps > 0);
    if (!ktls_present) return skipNoKtls(@src());
    const k_hs = try scopingRow(gpa, io, &id_a, .ktls, now, 50, 1 << 20, 1 << 16);
    const k_64k = try scopingRow(gpa, io, &id_a, .ktls, now, 1, 512 << 20, 1 << 16);
    const k_1m = try scopingRow(gpa, io, &id_a, .ktls, now, 1, 512 << 20, 1 << 20);
    std.debug.print("  mutual_tls/KTLS: connect+handshake {d} µs; 64 KiB writes {d:.2} GB/s {d:.3} CPU-s/GB; 1 MiB writes {d:.2} GB/s {d:.3} CPU-s/GB\n", .{ k_hs.handshake_us, k_64k.gbps, k_64k.cpu_s_per_gb, k_1m.gbps, k_1m.cpu_s_per_gb });
    try testing.expect(k_64k.gbps > 0 and k_1m.gbps > 0);
}

// ---------------------------------------------------------------------------
// The wire over MutualTls: the listener's and the dialer's arms.
// ---------------------------------------------------------------------------

const service = @import("service.zig");
const verbs = @import("verbs.zig");
const auth = @import("auth.zig");
const endpoint = @import("endpoint.zig");
const listener = @import("listener.zig");
const stats = @import("stats.zig");

const Echo = struct {
    served: std.atomic.Value(u64) = .init(0),

    fn serve(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *Echo = @ptrCast(@alignCast(ctx));
        _ = self.served.fetchAdd(1, .monotonic);
        switch (req.payload) {
            .resolve_slot => |r| return .{ .ok = .{ .resolve_slot = .{ .kind = 0, .reserved = @splat(0), .appender_id = 0, .g = r.slot, .reserved2 = 0 } } },
            else => return .{ .status = .{ .status = .not_manager } },
        }
    }
};

const wire_secret: auth.Secret = @splat(0x7A);

fn wireIdentity(n: u32) verbs.WireIdentity {
    return .{ .node_token = 0xBEEF00 + n, .writer_id = 1, .mount_slot = n, .reserved = 0 };
}

fn startWireListener(svc: *service.Service, posture: tls.Posture) !*listener.Listener {
    return listener.Listener.start(testing.allocator, testing.io, .{ .explicit = try endpoint.parseEndpoint("127.0.0.1:0") }, svc, wire_secret, .{ .raw_cpus = 4, .nofile = 1 << 16, .posture = posture });
}

test "wire e2e over MutualTls, the kernel record layer's law: the kTLS probe names this kernel's posture → WITHOUT the ULP a MutualTls dial completes the handshake and is REFUSED by the record layer on both ends (counted ktls_refused; never the userspace layer); WITH it the ladder rides inside kernel TLS (1,000 verbs on one connection, authenticated ∧ confidential, 2 handshakes + 1 admission) and a client under a FOREIGN CA is refused and counted untrusted → a Plaintext dialer is answered with a Refused naming the class at half the dial bound and counted → a MutualTls dialer at a Plaintext listener is refused loud and counted" {
    const io = testing.io;
    const gpa = testing.allocator;
    const now = clock.realtimeCoarseS();
    const tp = try tls.TestPki.init(io, now);
    var id_a = try tp.identity(gpa, .a);
    defer id_a.deinit(gpa);
    var id_b = try tp.identity(gpa, .b);
    defer id_b.deinit(gpa);
    var echo: Echo = .{};
    var svc: service.Service = .init;
    svc.register(.resolve_slot, .{ .ctx = &echo, .serve = &Echo.serve });
    const probe = tls.ktls.probeKernel();
    var ulps: [256]u8 = undefined;
    std.debug.print("\nwire e2e: kernel TLS probe: {t} (tcp_available_ulp: '{s}')\n", .{ probe, tls.ktls.availableUlps(&ulps) });
    const l = try startWireListener(&svc, .{ .mutual_tls = &id_a });
    defer l.stop();
    if (probe == .present) {
        var id_x = try tp.identity(gpa, .foreign);
        defer id_x.deinit(gpa);
        const hs0 = stats.cluster_wire_tls_handshakes.load();
        const adm0 = stats.cluster_wire_sessions_admitted.load();
        {
            var client = dialer.Client.init(gpa, io, l.advertised, wire_secret, wireIdentity(1));
            client.posture = .{ .mutual_tls = &id_b };
            defer client.deinit();
            var out: [256]u8 = undefined;
            var i: u16 = 0;
            while (i < 1000) : (i += 1) {
                const rep = try client.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = i, .reserved = 0 } }, &out);
                try testing.expectEqual(i, rep.resolve_slot.g);
            }
            try testing.expectEqual(tls.Channel.mutual_tls, client.mux.?.channel());
            try testing.expect(client.mux.?.sess.framer.authenticated() and client.mux.?.sess.framer.confidential());
            try testing.expect(client.mux.?.tls_conn.?.ktls_tx and client.mux.?.tls_conn.?.ktls_rx);
            try testing.expectEqual(@as(u64, 1000), echo.served.load(.monotonic));
            try testing.expectEqual(hs0 + 2, stats.cluster_wire_tls_handshakes.load());
            try testing.expectEqual(adm0 + 1, stats.cluster_wire_sessions_admitted.load());
            std.debug.print("wire e2e [ktls]: 1,000 verbs over MutualTls on one kernel-TLS connection\n", .{});
        }
        {
            const untrusted0 = stats.cluster_wire_tls_refused_untrusted.load();
            const dial_refused0 = stats.cluster_wire_tls_dial_refused.load();
            var client = dialer.Client.init(gpa, io, l.advertised, wire_secret, wireIdentity(2));
            client.posture = .{ .mutual_tls = &id_x };
            defer client.deinit();
            var out: [256]u8 = undefined;
            try testing.expectError(error.HandshakeRefused, client.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 1, .reserved = 0 } }, &out));
            try testing.expectEqual(dial_refused0 + 1, stats.cluster_wire_tls_dial_refused.load());
            const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
            while (stats.cluster_wire_tls_refused_untrusted.load() == untrusted0 and clock.monotonicNs() < deadline) io.sleep(.fromMilliseconds(10), .awake) catch {};
            try testing.expectEqual(untrusted0 + 1, stats.cluster_wire_tls_refused_untrusted.load());
        }
    } else {
        // Without the ULP: the handshake completes (both sides count it),
        // then the kernel record layer is armed — and refused by name, both
        // ends, counted; the session never admits.
        const hs0 = stats.cluster_wire_tls_handshakes.load();
        const kt0 = stats.cluster_wire_ktls_refused.load();
        const adm0 = stats.cluster_wire_sessions_admitted.load();
        var client = dialer.Client.init(gpa, io, l.advertised, wire_secret, wireIdentity(1));
        client.posture = .{ .mutual_tls = &id_b };
        defer client.deinit();
        var out: [256]u8 = undefined;
        try testing.expectError(error.HandshakeRefused, client.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 1, .reserved = 0 } }, &out));
        const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
        while (stats.cluster_wire_ktls_refused.load() < kt0 + 2 and clock.monotonicNs() < deadline) io.sleep(.fromMilliseconds(10), .awake) catch {};
        // The dial side refused first (it arms after its Finished); the
        // listener refused after reading that Finished: two counts.
        try testing.expectEqual(kt0 + 2, stats.cluster_wire_ktls_refused.load());
        try testing.expectEqual(adm0, stats.cluster_wire_sessions_admitted.load());
        // The handshakes themselves completed on at least the dial side
        // (the listener's completes before its arm fails too).
        try testing.expect(stats.cluster_wire_tls_handshakes.load() >= hs0);
        try testing.expectEqual(@as(usize, 0), echo.served.load(.monotonic));
    }
    // A Plaintext dialer: silent for the first half-bound, answered with a
    // plaintext Refused naming the class — before any record layer.
    {
        const pt0 = stats.cluster_wire_plaintext_refused.load();
        var client = dialer.Client.init(gpa, io, l.advertised, wire_secret, wireIdentity(3));
        client.posture = .plaintext;
        defer client.deinit();
        var out: [256]u8 = undefined;
        const t0 = clock.monotonicNs();
        try testing.expectError(error.HandshakeRefused, client.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 1, .reserved = 0 } }, &out));
        const waited_ms = (clock.monotonicNs() - t0) / std.time.ns_per_ms;
        try testing.expect(waited_ms >= session.DIAL_TIMEOUT_MS / 2 - 500 and waited_ms < session.DIAL_TIMEOUT_MS);
        try testing.expectEqual(pt0 + 1, stats.cluster_wire_plaintext_refused.load());
        std.debug.print("wire e2e: a Plaintext dialer at the MutualTls listener: Refused by name after {d} ms (half the dial bound), counted\n", .{waited_ms});
    }
    // A MutualTls dialer at a Plaintext listener: the Challenge frame
    // where the ServerHello should be — before any record layer.
    {
        const pl = try startWireListener(&svc, .plaintext);
        defer pl.stop();
        const dial_refused0 = stats.cluster_wire_tls_dial_refused.load();
        var client = dialer.Client.init(gpa, io, pl.advertised, wire_secret, wireIdentity(4));
        client.posture = .{ .mutual_tls = &id_b };
        defer client.deinit();
        var out: [256]u8 = undefined;
        try testing.expectError(error.HandshakeRefused, client.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 1, .reserved = 0 } }, &out));
        try testing.expectEqual(dial_refused0 + 1, stats.cluster_wire_tls_dial_refused.load());
        // And the plaintext client still works there, unconfidential.
        var plain = dialer.Client.init(gpa, io, pl.advertised, wire_secret, wireIdentity(5));
        plain.posture = .plaintext;
        defer plain.deinit();
        const rep = try plain.callTyped(.{ .resolve_slot = .{ .volume = 0, .slot = 9, .reserved = 0 } }, &out);
        try testing.expectEqual(@as(u32, 9), rep.resolve_slot.g);
        try testing.expect(plain.mux.?.sess.framer.authenticated() and !plain.mux.?.sess.framer.confidential());
    }
    if (probe == .absent) return skipNoKtls(@src());
}
