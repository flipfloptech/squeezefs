//! The `MutualTls` channel class (charter §8 "remote workers over TCP/TLS";
//! `docs/porting/sym_wire_membership_recovery.md` §1.1 channel classes;
//! decision 0003 §N): ONE TLS 1.3 profile, written here over `std.crypto`'s
//! primitives — `TLS_AES_128_GCM_SHA256`, X25519, Ed25519 certificates
//! under the cluster's own CA (`wire/pki.zig`) — with mutual
//! authentication (the server REQUIRES a client certificate under the
//! pinned CA; the client REQUIRES the server to ask for one and to present
//! one), the RFC 8446 §7.5 exporter that binds the storage-trust session
//! key to THIS channel (`auth.sessionKey`'s `binding`), and a record layer
//! that frames once per wire frame with no allocation per record.
//!
//! Why written and not vendored (0003 §N): Zig 0.17's `std.crypto.tls`
//! ships a CLIENT only — no server, no client-certificate arm (a
//! `CertificateRequest` is `TlsUnexpectedMessage`), no exporter; BearSSL
//! has no TLS 1.3; mbedTLS would bring a 100-file C tree and a config
//! whose job is to switch OFF everything this wire must never negotiate.
//! One profile is the design: there is nothing to downgrade to. A peer
//! that offers no common suite, group or scheme is refused by name.
//!
//! What is deliberately absent: TLS 1.2, resumption and tickets (a
//! `NewSessionTicket` from a foreign server is ignored), 0-RTT, HelloRetry
//! (one group, offered by every peer we accept), renegotiation, ALPN, SNI,
//! middlebox-compatibility CCS (received ones are dropped per RFC 8446
//! D.4). Present: KeyUpdate in both directions (a long-lived standing
//! session must never exhaust an AES-GCM key), close_notify, fatal alerts.
//!
//! The channel's trust is the PKI's: a peer's leaf must verify under the
//! configured CA (issuer name, Ed25519 signature, validity at the
//! handshake), and its CertificateVerify must verify under that leaf's
//! key; the leaf's `CN` is the peer's node name for the logs. A CA-less
//! configuration is refused at install, never downgraded. The per-frame
//! HMAC stays on top of TLS (defence in depth per the spec): TLS makes the
//! channel confidential, the exporter-bound MAC keeps a frame from being
//! replayed across channels.
//!
//! Portable: the engine, the identity and the in-memory tests; the file
//! loaders and the posture installer are Linux (the key file's 0600 law).

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const tls = std.crypto.tls;
const Sha256 = std.crypto.hash.sha2.Sha256;
const Aead = std.crypto.aead.aes_gcm.Aes128Gcm;
const Hmac = std.crypto.auth.hmac.sha2.HmacSha256;
const Hkdf = std.crypto.kdf.hkdf.HkdfSha256;
const X25519 = std.crypto.dh.X25519;
const fuzzkit = @import("../core/fuzzkit.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const knobs = @import("../core/knobs.zig");
const pki = @import("pki.zig");
const session = @import("session.zig");
const frame = @import("frame.zig");
const stats = @import("stats.zig");

const is_linux = builtin.os.tag == .linux;

/// The two channel classes (spec CW:987–1008): `plaintext` = the
/// storage-trust ladder alone (authenticated, not confidential);
/// `mutual_tls` = the ladder over this engine (authenticated AND
/// confidential). The wire word is the `.stats` gauge's value.
pub const Channel = enum(u8) {
    plaintext = 0,
    mutual_tls = 1,

    pub fn parse(text: []const u8) ?Channel {
        if (std.mem.eql(u8, text, "plaintext")) return .plaintext;
        if (std.mem.eql(u8, text, "mutual_tls")) return .mutual_tls;
        return null;
    }

    pub fn confidential(self: Channel) bool {
        return self == .mutual_tls;
    }
};

// ---------------------------------------------------------------------------
// The profile.
// ---------------------------------------------------------------------------

pub const SUITE: tls.CipherSuite = .AES_128_GCM_SHA256;
pub const GROUP: tls.NamedGroup = .x25519;
pub const SCHEME: tls.SignatureScheme = .ed25519;
pub const VERSION: tls.ProtocolVersion = .tls_1_3;

pub const HASH_BYTES: usize = Sha256.digest_length;
pub const KEY_BYTES: usize = Aead.key_length;
pub const IV_BYTES: usize = Aead.nonce_length;
pub const TAG_BYTES: usize = Aead.tag_length;
pub const RECORD_HEADER: usize = tls.record_header_len;
/// RFC 8446 §5.1: a plaintext fragment is at most 2^14 bytes.
pub const MAX_PLAINTEXT: usize = tls.max_ciphertext_inner_record_len;
/// §5.2: the inner plaintext (content ‖ type ‖ padding) plus the tag is at
/// most 2^14 + 256.
pub const MAX_CIPHERTEXT: usize = tls.max_ciphertext_len;
/// A handshake flight this engine accepts: one maximal record holds every
/// message of the profile (our flights are under 1 KiB; a foreign chain
/// wider than this is refused by the buffer, then by the profile).
pub const HANDSHAKE_BUF: usize = 2 * MAX_PLAINTEXT;
/// Our sending key rotates after this many records: RFC 8446 §5.5 bounds
/// AES-GCM at 2^24.5 full-size records per key; 2^23 is a comfortable half
/// (137 GB of 16 KiB records per direction).
pub const KEY_UPDATE_RECORDS: u64 = 1 << 23;
/// The exporter label the session key is bound to (`auth.BINDING_LABEL`).
pub const BINDING_LABEL = "squeezefs-cluster-wire/v1/binding";
pub const BINDING_BYTES: usize = 32;

comptime {
    std.debug.assert(HASH_BYTES == 32 and KEY_BYTES == 16 and IV_BYTES == 12 and TAG_BYTES == 16);
    std.debug.assert(RECORD_HEADER == 5 and MAX_PLAINTEXT == 1 << 14);
    std.debug.assert(BINDING_LABEL.len <= 249); // §7.5: the label rides HkdfLabel's 255 − "tls13 ".len
}

/// The record header's legacy version: `0x0303` on every record after the
/// ClientHello (RFC 8446 §5.1), which carries `0x0301` on its own.
const RECORD_VERSION: tls.ProtocolVersion = .tls_1_2;
const HELLO_RECORD_VERSION: tls.ProtocolVersion = .tls_1_0;
/// `legacy_version` inside the hellos.
const LEGACY_VERSION: tls.ProtocolVersion = .tls_1_2;

// ---------------------------------------------------------------------------
// The identity: the pinned CA, this node's leaf and its key.
// ---------------------------------------------------------------------------

pub const IdentityError = error{ CertificateMalformed, CertificateNotEd25519, CertificateTooLarge, LeafNotUnderCa, LeafExpired, LeafNotYetValid, KeyMismatch, OutOfMemory };

pub const Identity = struct {
    ca_der: []u8,
    cert_der: []u8,
    ca: pki.Parsed,
    cert: pki.Parsed,
    key: pki.KeyPair,

    /// Copies the DERs; checks the leaf under the CA at `now_s` and the
    /// key against the leaf — a node whose own material does not verify
    /// refuses at install, not at its first peer.
    pub fn init(gpa: std.mem.Allocator, ca_der: []const u8, cert_der: []const u8, key: pki.KeyPair, now_s: u64) IdentityError!Identity {
        const ca_copy = try gpa.dupe(u8, ca_der);
        errdefer gpa.free(ca_copy);
        const cert_copy = try gpa.dupe(u8, cert_der);
        errdefer gpa.free(cert_copy);
        const ca = try pki.parseCertificate(ca_copy);
        const cert = try pki.parseCertificate(cert_copy);
        pki.verifyLeaf(&cert, &ca, now_s) catch |e| return switch (e) {
            error.Untrusted => error.LeafNotUnderCa,
            error.Expired => error.LeafExpired,
            error.NotYetValid => error.LeafNotYetValid,
        };
        if (!std.mem.eql(u8, cert.publicKey(), &key.public_key.toBytes())) return error.KeyMismatch;
        return .{ .ca_der = ca_copy, .cert_der = cert_copy, .ca = ca, .cert = cert, .key = key };
    }

    pub fn deinit(self: *Identity, gpa: std.mem.Allocator) void {
        gpa.free(self.ca_der);
        gpa.free(self.cert_der);
        std.crypto.secureZero(u8, std.mem.asBytes(&self.key));
        self.* = undefined;
    }

    pub fn nodeName(self: *const Identity) []const u8 {
        return self.cert.commonName();
    }
};

// ---------------------------------------------------------------------------
// The process posture: one daemon, one listener, one identity.
// ---------------------------------------------------------------------------

pub const Posture = union(enum) {
    plaintext,
    mutual_tls: *const Identity,

    pub fn channel(self: Posture) Channel {
        return switch (self) {
            .plaintext => .plaintext,
            .mutual_tls => .mutual_tls,
        };
    }
};

/// Installed once at bring-up (`installFromEnv`) before the listener binds
/// or the first dial; read with acquire by every connection (pairs with
/// `install`'s release store).
var installed: std.atomic.Value(?*const Identity) = .init(null);

pub fn install(identity: ?*const Identity) void {
    installed.store(identity, .release);
    stats.cluster_wire_channel_mutual_tls.set(@intFromBool(identity != null));
}

pub fn posture() Posture {
    if (installed.load(.acquire)) |id| return .{ .mutual_tls = id };
    return .plaintext;
}

// ---------------------------------------------------------------------------
// The record layer.
// ---------------------------------------------------------------------------

/// One direction's traffic keys (RFC 8446 §7.3) and its record sequence.
const Traffic = struct {
    secret: [HASH_BYTES]u8,
    key: [KEY_BYTES]u8,
    iv: [IV_BYTES]u8,
    seq: u64 = 0,

    fn fromSecret(secret: [HASH_BYTES]u8) Traffic {
        return .{
            .secret = secret,
            .key = tls.hkdfExpandLabel(Hkdf, secret, "key", "", KEY_BYTES),
            .iv = tls.hkdfExpandLabel(Hkdf, secret, "iv", "", IV_BYTES),
        };
    }

    /// §5.3: the per-record nonce is the IV XOR the sequence, left-padded.
    fn nonce(self: *const Traffic) [IV_BYTES]u8 {
        var n = self.iv;
        var seq_be: [8]u8 = undefined;
        std.mem.writeInt(u64, &seq_be, self.seq, .big);
        for (seq_be, 0..) |b, i| n[IV_BYTES - 8 + i] ^= b;
        return n;
    }

    /// §7.2: the next generation of this direction's secret.
    fn updated(self: *const Traffic) Traffic {
        return fromSecret(tls.hkdfExpandLabel(Hkdf, self.secret, "traffic upd", "", HASH_BYTES));
    }

    fn zeroize(self: *Traffic) void {
        std.crypto.secureZero(u8, std.mem.asBytes(self));
    }
};

pub const HandshakeError = error{
    /// The peer's first bytes are the plaintext frame magic: a Plaintext
    /// dial into a MutualTls listener (or the reverse).
    PeerPlaintext,
    /// Not a TLS record at all.
    PeerNotTls,
    /// A malformed or out-of-order handshake message.
    Protocol,
    /// No common version / suite / group / scheme with the peer.
    ProfileMismatch,
    /// The peer presented no certificate (a server that did not ask for
    /// ours is the same refusal: the class is MUTUAL).
    PeerCertificateMissing,
    PeerCertificateUntrusted,
    PeerCertificateExpired,
    PeerCertificateNotYetValid,
    /// CertificateVerify or Finished did not verify.
    PeerSignatureInvalid,
    /// The peer sent a fatal alert.
    Alert,
    Transport,
    Timeout,
    Eof,
    OutOfMemory,
};

pub const Role = enum { server, client };

/// One established (or establishing) connection. Heap-allocated once per
/// connection; its buffers are the connection's for life.
pub const Conn = struct {
    inner: session.Link,
    role: Role,
    rd: ?Traffic = null,
    wr: ?Traffic = null,
    /// The decrypted bytes not yet handed to the reader.
    rd_plain: [MAX_CIPHERTEXT]u8 = undefined,
    rd_start: usize = 0,
    rd_end: usize = 0,
    /// The ciphertext record being read (header + body).
    rd_cipher: [RECORD_HEADER + MAX_CIPHERTEXT]u8 = undefined,
    /// The record being written (header + content + type + tag).
    wr_buf: [RECORD_HEADER + MAX_PLAINTEXT + 1 + TAG_BYTES]u8 = undefined,
    exporter_master: [HASH_BYTES]u8 = @splat(0),
    peer_name: [pki.NAME_MAX]u8 = @splat(0),
    peer_name_len: u8 = 0,
    established: bool = false,
    /// Set by the reader on a KeyUpdate(update_requested); honoured by the
    /// next write under the writer's own serialization (a reader never
    /// writes: the mux's halves are owned by different threads).
    peer_update_requested: std.atomic.Value(bool) = .init(false),
    close_notify_sent: bool = false,
    /// A KeyUpdate from our side rotated the write key (counted).
    key_updates_sent: u32 = 0,
    key_updates_received: u32 = 0,

    pub fn peerName(self: *const Conn) []const u8 {
        return self.peer_name[0..self.peer_name_len];
    }

    /// RFC 8446 §7.5: `TLS-Exporter(label, "", 32)`.
    pub fn exporter(self: *const Conn, label: []const u8, out: *[BINDING_BYTES]u8) void {
        std.debug.assert(self.established);
        const empty = tls.emptyHash(Sha256);
        const derived = tls.hkdfExpandLabel(Hkdf, self.exporter_master, label, &empty, HASH_BYTES);
        out.* = tls.hkdfExpandLabel(Hkdf, derived, "exporter", &empty, BINDING_BYTES);
    }

    /// The channel-binding word the session key is derived under.
    pub fn binding(self: *const Conn) [BINDING_BYTES]u8 {
        var out: [BINDING_BYTES]u8 = undefined;
        self.exporter(BINDING_LABEL, &out);
        return out;
    }

    pub fn link(self: *Conn) session.Link {
        return .{ .ctx = self, .vtable = &vtable };
    }

    pub fn destroy(self: *Conn, gpa: std.mem.Allocator) void {
        if (self.rd) |*t| t.zeroize();
        if (self.wr) |*t| t.zeroize();
        std.crypto.secureZero(u8, &self.exporter_master);
        gpa.destroy(self);
    }

    const vtable: session.Link.VTable = .{ .readExact = linkRead, .writeAll = linkWrite, .shutdown = linkShutdown, .close = linkClose };

    // ---- the raw record I/O ----

    /// Read one record from the peer: header, then body; a record wider
    /// than the ceiling is `Protocol` (RFC 8446 §5.2 `record_overflow`).
    fn readRawRecord(self: *Conn, timeout_ms: ?u32) HandshakeError!struct { ct: tls.ContentType, body: []u8 } {
        const hdr = self.rd_cipher[0..RECORD_HEADER];
        self.inner.readExact(hdr, timeout_ms) catch |e| return mapLink(e);
        // The frame magic where a record header should be: a peer of the
        // other channel class (judged before the length word, which is
        // the magic's own bytes).
        if (std.mem.eql(u8, hdr[0..frame.MAGIC.len], &frame.MAGIC)) return error.PeerPlaintext;
        const ct = contentType(hdr[0]) orelse return error.PeerNotTls;
        const len = std.mem.readInt(u16, hdr[3..5], .big);
        if (len > MAX_CIPHERTEXT) return error.Protocol;
        const body = self.rd_cipher[RECORD_HEADER..][0..len];
        self.inner.readExact(body, timeout_ms) catch |e| return mapLink(e);
        return .{ .ct = ct, .body = body };
    }

    /// The next record's inner content type and plaintext (into
    /// `rd_plain`), decrypted under the read keys when they are armed;
    /// change_cipher_spec records are dropped (RFC 8446 D.4).
    fn readRecord(self: *Conn, timeout_ms: ?u32) HandshakeError!struct { ct: tls.ContentType, plain: []u8 } {
        while (true) {
            const raw = try self.readRawRecord(timeout_ms);
            if (raw.ct == .change_cipher_spec) {
                if (raw.body.len != 1 or raw.body[0] != 1) return error.Protocol;
                continue;
            }
            if (self.rd) |*t| {
                if (raw.ct != .application_data) {
                    // A cleartext alert after keys are armed is a peer that
                    // gave up mid-handshake (OpenSSL's `s_client` on a
                    // refused certificate): report it as such.
                    if (raw.ct == .alert) return error.Alert;
                    return error.Protocol;
                }
                if (raw.body.len < TAG_BYTES + 1) return error.Protocol;
                const c = raw.body[0 .. raw.body.len - TAG_BYTES];
                const tag = raw.body[raw.body.len - TAG_BYTES ..][0..TAG_BYTES];
                const m = self.rd_plain[0..c.len];
                Aead.decrypt(m, c, tag.*, self.rd_cipher[0..RECORD_HEADER], t.nonce(), t.key) catch return error.PeerSignatureInvalid;
                t.seq += 1;
                // §5.4: strip the zero padding; the last non-zero byte is
                // the content type.
                var n = m.len;
                while (n > 0 and m[n - 1] == 0) n -= 1;
                if (n == 0) return error.Protocol;
                const ct = contentType(m[n - 1]) orelse return error.Protocol;
                return .{ .ct = ct, .plain = m[0 .. n - 1] };
            }
            @memcpy(self.rd_plain[0..raw.body.len], raw.body);
            return .{ .ct = raw.ct, .plain = self.rd_plain[0..raw.body.len] };
        }
    }

    /// Write one record of `plain` (≤ MAX_PLAINTEXT) as content type `ct`:
    /// cleartext before the keys are armed, else AEAD-sealed with the type
    /// inside. One `writeAll` per record.
    fn writeRecord(self: *Conn, ct: tls.ContentType, plain: []const u8) HandshakeError!void {
        std.debug.assert(plain.len <= MAX_PLAINTEXT);
        const hdr = self.wr_buf[0..RECORD_HEADER];
        if (self.wr) |*t| {
            const inner_len = plain.len + 1;
            hdr[0] = @backingInt(tls.ContentType.application_data);
            std.mem.writeInt(u16, hdr[1..3], @backingInt(RECORD_VERSION), .big);
            std.mem.writeInt(u16, hdr[3..5], @intCast(inner_len + TAG_BYTES), .big);
            // The inner plaintext is built in place (content ‖ type) and
            // sealed in place.
            const body = self.wr_buf[RECORD_HEADER..][0..inner_len];
            @memcpy(body[0..plain.len], plain);
            body[plain.len] = @backingInt(ct);
            const tag = self.wr_buf[RECORD_HEADER + inner_len ..][0..TAG_BYTES];
            Aead.encrypt(body, tag, body, hdr, t.nonce(), t.key);
            t.seq += 1;
            self.inner.writeAll(self.wr_buf[0 .. RECORD_HEADER + inner_len + TAG_BYTES]) catch |e| return mapLink(e);
            return;
        }
        hdr[0] = @backingInt(ct);
        std.mem.writeInt(u16, hdr[1..3], @backingInt(if (ct == .handshake and self.role == .client) HELLO_RECORD_VERSION else RECORD_VERSION), .big);
        std.mem.writeInt(u16, hdr[3..5], @intCast(plain.len), .big);
        @memcpy(self.wr_buf[RECORD_HEADER..][0..plain.len], plain);
        self.inner.writeAll(self.wr_buf[0 .. RECORD_HEADER + plain.len]) catch |e| return mapLink(e);
    }

    fn writeAlert(self: *Conn, level: tls.Alert.Level, desc: tls.Alert.Description) void {
        self.writeRecord(.alert, &.{ @backingInt(level), @backingInt(desc) }) catch {};
    }

    /// §4.6.3: rotate our write key, telling the peer first.
    fn sendKeyUpdate(self: *Conn) HandshakeError!void {
        const msg = [_]u8{ @backingInt(tls.HandshakeType.key_update), 0, 0, 1, @backingInt(tls.KeyUpdateRequest.update_not_requested) };
        try self.writeRecord(.handshake, &msg);
        var next = self.wr.?.updated();
        self.wr.?.zeroize();
        self.wr = next;
        next.zeroize();
        self.key_updates_sent += 1;
        stats.cluster_wire_tls_key_updates.inc();
    }

    /// A post-handshake message on the read side: KeyUpdate rotates the
    /// read key (and asks the writer to rotate ours when requested);
    /// NewSessionTicket is a foreign server's and ignored.
    fn postHandshake(self: *Conn, plain: []const u8) HandshakeError!void {
        var cur: Cur = .{ .bytes = plain };
        while (!cur.done()) {
            const ht = try cur.byte();
            const len = try cur.word24();
            const body = try cur.take(len);
            switch (ht) {
                @backingInt(tls.HandshakeType.key_update) => {
                    if (body.len != 1) return error.Protocol;
                    const req = std.enums.fromInt(tls.KeyUpdateRequest, body[0]) orelse return error.Protocol;
                    var next = self.rd.?.updated();
                    self.rd.?.zeroize();
                    self.rd = next;
                    next.zeroize();
                    self.key_updates_received += 1;
                    if (req == .update_requested) self.peer_update_requested.store(true, .release);
                },
                @backingInt(tls.HandshakeType.new_session_ticket) => {},
                else => return error.Protocol,
            }
        }
    }

    // ---- the Link face ----

    fn linkRead(ctx: *anyopaque, buf: []u8, timeout_ms: ?u32) session.FrameError!void {
        const self: *Conn = @ptrCast(@alignCast(ctx));
        var filled: usize = 0;
        while (filled < buf.len) {
            if (self.rd_start == self.rd_end) {
                const rec = self.readRecord(timeout_ms) catch |e| return mapHandshake(e);
                switch (rec.ct) {
                    .application_data => {
                        self.rd_start = 0;
                        self.rd_end = rec.plain.len;
                    },
                    .handshake => {
                        self.postHandshake(rec.plain) catch |e| return mapHandshake(e);
                        continue;
                    },
                    .alert => {
                        // close_notify = an orderly EOF; anything else a
                        // peer that tore the session down.
                        if (rec.plain.len == 2 and rec.plain[1] == @backingInt(tls.Alert.Description.close_notify)) return error.Eof;
                        return error.TransportFailed;
                    },
                    else => return error.TransportFailed,
                }
                continue;
            }
            const n = @min(buf.len - filled, self.rd_end - self.rd_start);
            @memcpy(buf[filled..][0..n], self.rd_plain[self.rd_start..][0..n]);
            self.rd_start += n;
            filled += n;
        }
    }

    /// One wire frame → ⌈len / 2^14⌉ records, each one `writeAll` on the
    /// socket; a pending KeyUpdate (ours by count, or the peer's request)
    /// goes out first.
    fn linkWrite(ctx: *anyopaque, bytes: []const u8) session.FrameError!void {
        const self: *Conn = @ptrCast(@alignCast(ctx));
        if (self.peer_update_requested.swap(false, .acq_rel) or self.wr.?.seq >= KEY_UPDATE_RECORDS) {
            self.sendKeyUpdate() catch |e| return mapHandshake(e);
        }
        var off: usize = 0;
        while (off < bytes.len) {
            const n = @min(bytes.len - off, MAX_PLAINTEXT);
            self.writeRecord(.application_data, bytes[off..][0..n]) catch |e| return mapHandshake(e);
            off += n;
        }
    }

    fn linkShutdown(ctx: *anyopaque) void {
        const self: *Conn = @ptrCast(@alignCast(ctx));
        self.inner.shutdown();
    }

    /// close_notify (best effort: the socket may be gone) then the socket.
    fn linkClose(ctx: *anyopaque) void {
        const self: *Conn = @ptrCast(@alignCast(ctx));
        if (self.established and !self.close_notify_sent) {
            self.close_notify_sent = true;
            self.writeAlert(.warning, .close_notify);
        }
        self.inner.close();
    }
};

/// The four content types a record may carry (`tls.ContentType` is
/// non-exhaustive; anything else is not TLS).
fn contentType(b: u8) ?tls.ContentType {
    return switch (b) {
        @backingInt(tls.ContentType.change_cipher_spec) => .change_cipher_spec,
        @backingInt(tls.ContentType.alert) => .alert,
        @backingInt(tls.ContentType.handshake) => .handshake,
        @backingInt(tls.ContentType.application_data) => .application_data,
        else => null,
    };
}

fn mapLink(e: session.FrameError) HandshakeError {
    return switch (e) {
        error.Eof => error.Eof,
        error.Timeout => error.Timeout,
        else => error.Transport,
    };
}

fn mapHandshake(e: HandshakeError) session.FrameError {
    return switch (e) {
        error.Eof => error.Eof,
        error.Timeout => error.Timeout,
        else => error.TransportFailed,
    };
}

// ---------------------------------------------------------------------------
// Byte cursors and builders (total; the fuzz target).
// ---------------------------------------------------------------------------

const Cur = struct {
    bytes: []const u8,
    i: usize = 0,

    fn done(self: *const Cur) bool {
        return self.i >= self.bytes.len;
    }
    fn take(self: *Cur, n: usize) error{Protocol}![]const u8 {
        if (self.bytes.len - self.i < n) return error.Protocol;
        const s = self.bytes[self.i..][0..n];
        self.i += n;
        return s;
    }
    fn byte(self: *Cur) error{Protocol}!u8 {
        return (try self.take(1))[0];
    }
    fn word(self: *Cur) error{Protocol}!u16 {
        return std.mem.readInt(u16, (try self.take(2))[0..2], .big);
    }
    fn word24(self: *Cur) error{Protocol}!u24 {
        return std.mem.readInt(u24, (try self.take(3))[0..3], .big);
    }
    /// A `<0..2^16-1>` vector.
    fn vec16(self: *Cur) error{Protocol}![]const u8 {
        return self.take(try self.word());
    }
    fn vec8(self: *Cur) error{Protocol}![]const u8 {
        return self.take(try self.byte());
    }
};

const Out = struct {
    buf: []u8,
    n: usize = 0,

    fn bytes(self: *Out, b: []const u8) error{Protocol}!void {
        if (self.buf.len - self.n < b.len) return error.Protocol;
        @memcpy(self.buf[self.n..][0..b.len], b);
        self.n += b.len;
    }
    fn byte(self: *Out, v: u8) error{Protocol}!void {
        try self.bytes(&.{v});
    }
    fn word(self: *Out, v: u16) error{Protocol}!void {
        var w: [2]u8 = undefined;
        std.mem.writeInt(u16, &w, v, .big);
        try self.bytes(&w);
    }
    fn word24(self: *Out, v: u24) error{Protocol}!void {
        var w: [3]u8 = undefined;
        std.mem.writeInt(u24, &w, v, .big);
        try self.bytes(&w);
    }
    /// Reserve a length prefix; `close*` fills it with the bytes since.
    fn open16(self: *Out) error{Protocol}!usize {
        try self.word(0);
        return self.n;
    }
    fn close16(self: *Out, mark: usize) void {
        std.mem.writeInt(u16, self.buf[mark - 2 ..][0..2], @intCast(self.n - mark), .big);
    }
    fn open24(self: *Out) error{Protocol}!usize {
        try self.word24(0);
        return self.n;
    }
    fn close24(self: *Out, mark: usize) void {
        std.mem.writeInt(u24, self.buf[mark - 3 ..][0..3], @intCast(self.n - mark), .big);
    }
    fn written(self: *const Out) []const u8 {
        return self.buf[0..self.n];
    }
};

/// What a ClientHello offers that the profile needs.
pub const ClientHello = struct {
    random: [32]u8,
    session_id: []const u8,
    /// The peer's X25519 share.
    key_share: ?[X25519.public_length]u8,
    offers_suite: bool,
    offers_group: bool,
    offers_scheme: bool,
    offers_version: bool,
    offers_null_compression: bool,
};

/// Total: the ClientHello body (after the 4 B handshake header).
pub fn parseClientHello(body: []const u8) error{Protocol}!ClientHello {
    var c: Cur = .{ .bytes = body };
    _ = try c.word(); // legacy_version (ignored: supported_versions decides)
    var out: ClientHello = .{ .random = (try c.take(32))[0..32].*, .session_id = &.{}, .key_share = null, .offers_suite = false, .offers_group = false, .offers_scheme = false, .offers_version = false, .offers_null_compression = false };
    out.session_id = try c.vec8();
    if (out.session_id.len > 32) return error.Protocol;
    const suites = try c.vec16();
    if (suites.len % 2 != 0) return error.Protocol;
    var si: usize = 0;
    while (si < suites.len) : (si += 2) {
        if (std.mem.readInt(u16, suites[si..][0..2], .big) == @backingInt(SUITE)) out.offers_suite = true;
    }
    const comp = try c.vec8();
    for (comp) |m| if (m == @backingInt(tls.CompressionMethod.null)) {
        out.offers_null_compression = true;
    };
    if (c.done()) return out; // no extensions: nothing offered
    const exts = try c.vec16();
    if (!c.done()) return error.Protocol;
    var e: Cur = .{ .bytes = exts };
    while (!e.done()) {
        const et = try e.word();
        const data = try e.vec16();
        var d: Cur = .{ .bytes = data };
        switch (et) {
            @backingInt(tls.ExtensionType.supported_versions) => {
                const vs = try d.vec8();
                if (vs.len % 2 != 0) return error.Protocol;
                var vi: usize = 0;
                while (vi < vs.len) : (vi += 2) {
                    if (std.mem.readInt(u16, vs[vi..][0..2], .big) == @backingInt(VERSION)) out.offers_version = true;
                }
            },
            @backingInt(tls.ExtensionType.supported_groups) => {
                const gs = try d.vec16();
                if (gs.len % 2 != 0) return error.Protocol;
                var gi: usize = 0;
                while (gi < gs.len) : (gi += 2) {
                    if (std.mem.readInt(u16, gs[gi..][0..2], .big) == @backingInt(GROUP)) out.offers_group = true;
                }
            },
            @backingInt(tls.ExtensionType.signature_algorithms) => {
                const ss = try d.vec16();
                if (ss.len % 2 != 0) return error.Protocol;
                var xi: usize = 0;
                while (xi < ss.len) : (xi += 2) {
                    if (std.mem.readInt(u16, ss[xi..][0..2], .big) == @backingInt(SCHEME)) out.offers_scheme = true;
                }
            },
            @backingInt(tls.ExtensionType.key_share) => {
                const shares = try d.vec16();
                var k: Cur = .{ .bytes = shares };
                while (!k.done()) {
                    const group = try k.word();
                    const kx = try k.vec16();
                    if (group == @backingInt(GROUP)) {
                        if (kx.len != X25519.public_length) return error.Protocol;
                        out.key_share = kx[0..X25519.public_length].*;
                    }
                }
            },
            else => {},
        }
    }
    return out;
}

pub const ServerHello = struct {
    random: [32]u8,
    session_id: []const u8,
    suite: u16,
    key_share: ?[X25519.public_length]u8,
    selected_version: ?u16,
};

/// Total: the ServerHello body.
pub fn parseServerHello(body: []const u8) error{Protocol}!ServerHello {
    var c: Cur = .{ .bytes = body };
    if (try c.word() != @backingInt(LEGACY_VERSION)) return error.Protocol;
    var out: ServerHello = .{ .random = (try c.take(32))[0..32].*, .session_id = &.{}, .suite = 0, .key_share = null, .selected_version = null };
    out.session_id = try c.vec8();
    out.suite = try c.word();
    if (try c.byte() != @backingInt(tls.CompressionMethod.null)) return error.Protocol;
    const exts = try c.vec16();
    if (!c.done()) return error.Protocol;
    var e: Cur = .{ .bytes = exts };
    while (!e.done()) {
        const et = try e.word();
        const data = try e.vec16();
        var d: Cur = .{ .bytes = data };
        switch (et) {
            @backingInt(tls.ExtensionType.supported_versions) => out.selected_version = try d.word(),
            @backingInt(tls.ExtensionType.key_share) => {
                const group = try d.word();
                const kx = try d.vec16();
                if (group != @backingInt(GROUP) or kx.len != X25519.public_length) return error.Protocol;
                out.key_share = kx[0..X25519.public_length].*;
            },
            else => {},
        }
    }
    return out;
}

/// Total: a TLS 1.3 Certificate body → the FIRST entry's DER (the leaf);
/// an empty list is `null`; the request context must be empty.
pub fn parseCertificateMsg(body: []const u8) error{Protocol}!?[]const u8 {
    var c: Cur = .{ .bytes = body };
    const ctx = try c.vec8();
    if (ctx.len != 0) return error.Protocol;
    const list = try c.take(try c.word24());
    if (!c.done()) return error.Protocol;
    if (list.len == 0) return null;
    var l: Cur = .{ .bytes = list };
    const der = try l.take(try l.word24());
    _ = try l.vec16(); // the entry's extensions (ignored)
    // Further entries (a chain) are not read: the profile verifies the leaf
    // under the pinned CA and nothing else.
    return der;
}

/// Total: CertificateVerify → the Ed25519 signature.
pub fn parseCertificateVerify(body: []const u8) error{ Protocol, ProfileMismatch }![pki.SIGNATURE_BYTES]u8 {
    var c: Cur = .{ .bytes = body };
    if (try c.word() != @backingInt(SCHEME)) return error.ProfileMismatch;
    const sig = try c.vec16();
    if (sig.len != pki.SIGNATURE_BYTES or !c.done()) return error.Protocol;
    return sig[0..pki.SIGNATURE_BYTES].*;
}

/// Total: CertificateRequest → whether Ed25519 is acceptable (the context
/// must be empty: we never resume).
pub fn parseCertificateRequest(body: []const u8) error{Protocol}!bool {
    var c: Cur = .{ .bytes = body };
    const ctx = try c.vec8();
    if (ctx.len != 0) return error.Protocol;
    const exts = try c.vec16();
    if (!c.done()) return error.Protocol;
    var e: Cur = .{ .bytes = exts };
    var ok = false;
    while (!e.done()) {
        const et = try e.word();
        const data = try e.vec16();
        if (et == @backingInt(tls.ExtensionType.signature_algorithms)) {
            var d: Cur = .{ .bytes = data };
            const ss = try d.vec16();
            if (ss.len % 2 != 0) return error.Protocol;
            var i: usize = 0;
            while (i < ss.len) : (i += 2) {
                if (std.mem.readInt(u16, ss[i..][0..2], .big) == @backingInt(SCHEME)) ok = true;
            }
        }
    }
    return ok;
}

// ---------------------------------------------------------------------------
// The handshake.
// ---------------------------------------------------------------------------

/// The key schedule's running state (RFC 8446 §7.1) and the handshake
/// message reader; allocated for the handshake's duration only.
const Handshake = struct {
    conn: *Conn,
    identity: *const Identity,
    io: Io,
    now_s: u64,
    timeout_ms: u32,
    transcript: Sha256 = .init(.{}),
    buf: [HANDSHAKE_BUF]u8 = undefined,
    /// Bytes of handshake messages received and not yet consumed.
    have: usize = 0,
    used: usize = 0,
    msg: [HANDSHAKE_BUF]u8 = undefined,
    ecdh: X25519.KeyPair = undefined,
    handshake_secret: [HASH_BYTES]u8 = undefined,
    master_secret: [HASH_BYTES]u8 = undefined,
    client_hs: [HASH_BYTES]u8 = undefined,
    server_hs: [HASH_BYTES]u8 = undefined,

    fn deinit(self: *Handshake) void {
        std.crypto.secureZero(u8, std.mem.asBytes(self));
    }

    fn th(self: *const Handshake) [HASH_BYTES]u8 {
        return self.transcript.peek();
    }

    /// §7.1 `Derive-Secret(secret, label, Messages)`.
    fn deriveSecret(secret: [HASH_BYTES]u8, label: []const u8, transcript_hash: [HASH_BYTES]u8) [HASH_BYTES]u8 {
        return tls.hkdfExpandLabel(Hkdf, secret, label, &transcript_hash, HASH_BYTES);
    }

    /// §4.4.4: `HMAC(finished_key, Transcript-Hash)`.
    fn finishedMac(traffic_secret: [HASH_BYTES]u8, transcript_hash: [HASH_BYTES]u8) [HASH_BYTES]u8 {
        const fk = tls.hkdfExpandLabel(Hkdf, traffic_secret, "finished", "", HASH_BYTES);
        return tls.hmac(Hmac, &transcript_hash, fk);
    }

    /// Read the next handshake message (type, body), reading records as
    /// needed; a message may span records. The body is valid until the
    /// next call.
    fn nextMessage(self: *Handshake) HandshakeError!struct { ht: u8, body: []const u8, raw: []const u8 } {
        while (true) {
            if (self.have - self.used >= 4) {
                const hdr = self.buf[self.used..][0..4];
                const len: usize = std.mem.readInt(u24, hdr[1..4], .big);
                if (4 + len > HANDSHAKE_BUF) return error.Protocol;
                if (self.have - self.used >= 4 + len) {
                    const raw = self.buf[self.used..][0 .. 4 + len];
                    @memcpy(self.msg[0..raw.len], raw);
                    self.used += 4 + len;
                    return .{ .ht = hdr[0], .body = self.msg[4..raw.len], .raw = self.msg[0..raw.len] };
                }
            }
            // Compact and read another record.
            if (self.used > 0) {
                std.mem.copyForwards(u8, self.buf[0 .. self.have - self.used], self.buf[self.used..self.have]);
                self.have -= self.used;
                self.used = 0;
            }
            const rec = try self.conn.readRecord(self.timeout_ms);
            switch (rec.ct) {
                .handshake => {},
                .alert => return error.Alert,
                else => return error.Protocol,
            }
            if (self.have + rec.plain.len > HANDSHAKE_BUF) return error.Protocol;
            @memcpy(self.buf[self.have..][0..rec.plain.len], rec.plain);
            self.have += rec.plain.len;
        }
    }

    fn expect(self: *Handshake, ht: tls.HandshakeType) HandshakeError![]const u8 {
        const m = try self.nextMessage();
        if (m.ht != @backingInt(ht)) return error.Protocol;
        self.transcript.update(m.raw);
        return m.body;
    }

    /// Send a flight of handshake messages built into `msg` as ONE record.
    fn sendFlight(self: *Handshake, flight: []const u8) HandshakeError!void {
        if (flight.len > MAX_PLAINTEXT) return error.Protocol;
        try self.conn.writeRecord(.handshake, flight);
    }

    /// §7.1 up to the handshake traffic secrets, after both hellos.
    fn deriveHandshakeSecrets(self: *Handshake, peer_share: [X25519.public_length]u8) HandshakeError!void {
        const shared = X25519.scalarmult(self.ecdh.secret_key, peer_share) catch return error.ProfileMismatch;
        const zeroes: [HASH_BYTES]u8 = @splat(0);
        const early = Hkdf.extract(&zeroes, &zeroes);
        const empty = tls.emptyHash(Sha256);
        const derived = deriveSecret(early, "derived", empty);
        self.handshake_secret = Hkdf.extract(&derived, &shared);
        const hello_hash = self.th();
        self.client_hs = deriveSecret(self.handshake_secret, "c hs traffic", hello_hash);
        self.server_hs = deriveSecret(self.handshake_secret, "s hs traffic", hello_hash);
        const derived2 = deriveSecret(self.handshake_secret, "derived", empty);
        self.master_secret = Hkdf.extract(&derived2, &zeroes);
    }

    /// The application secrets and the exporter master, at the transcript
    /// through the server's Finished.
    fn deriveApplicationSecrets(self: *Handshake) struct { client: [HASH_BYTES]u8, server: [HASH_BYTES]u8 } {
        const h = self.th();
        self.conn.exporter_master = deriveSecret(self.master_secret, "exp master", h);
        return .{ .client = deriveSecret(self.master_secret, "c ap traffic", h), .server = deriveSecret(self.master_secret, "s ap traffic", h) };
    }

    /// §4.4.3: the signed content of a CertificateVerify.
    fn certificateVerifyContent(role: Role, transcript_hash: [HASH_BYTES]u8, out: *[64 + 34 + HASH_BYTES]u8) []const u8 {
        const pad: [64]u8 = @splat(' ');
        const ctx: []const u8 = switch (role) {
            .server => "TLS 1.3, server CertificateVerify",
            .client => "TLS 1.3, client CertificateVerify",
        };
        out[0..64].* = pad;
        @memcpy(out[64..][0..ctx.len], ctx);
        out[64 + ctx.len] = 0;
        out[64 + ctx.len + 1 ..][0..HASH_BYTES].* = transcript_hash;
        return out[0 .. 64 + ctx.len + 1 + HASH_BYTES];
    }

    /// Certificate ‖ CertificateVerify ‖ Finished of OUR side into `o`,
    /// each appended to the transcript as it is built.
    fn buildAuthFlight(self: *Handshake, o: *Out, traffic_secret: [HASH_BYTES]u8) HandshakeError!void {
        // Certificate: context 0, one entry, no extensions.
        const cert_start = o.n;
        try o.byte(@backingInt(tls.HandshakeType.certificate));
        const cert_mark = try o.open24();
        try o.byte(0);
        const list_mark = try o.open24();
        try o.word24(@intCast(self.identity.cert_der.len));
        try o.bytes(self.identity.cert_der);
        try o.word(0);
        o.close24(list_mark);
        o.close24(cert_mark);
        self.transcript.update(o.buf[cert_start..o.n]);
        // CertificateVerify.
        var content: [64 + 34 + HASH_BYTES]u8 = undefined;
        const signed = certificateVerifyContent(self.conn.role, self.th(), &content);
        const sig = self.identity.key.sign(signed, null) catch return error.Protocol;
        const cv_start = o.n;
        try o.byte(@backingInt(tls.HandshakeType.certificate_verify));
        const cv_mark = try o.open24();
        try o.word(@backingInt(SCHEME));
        try o.word(pki.SIGNATURE_BYTES);
        try o.bytes(&sig.toBytes());
        o.close24(cv_mark);
        self.transcript.update(o.buf[cv_start..o.n]);
        // Finished.
        const mac = finishedMac(traffic_secret, self.th());
        const fin_start = o.n;
        try o.byte(@backingInt(tls.HandshakeType.finished));
        try o.word24(HASH_BYTES);
        try o.bytes(&mac);
        self.transcript.update(o.buf[fin_start..o.n]);
    }

    /// The peer's Certificate ‖ CertificateVerify ‖ Finished: the leaf
    /// under the pinned CA, the signature under the leaf, the MAC under
    /// the peer's handshake secret. The peer's name is recorded.
    fn readAuthFlight(self: *Handshake, peer_role: Role, traffic_secret: [HASH_BYTES]u8) HandshakeError!void {
        const cert_body = try self.expect(.certificate);
        const leaf_der = (try parseCertificateMsg(cert_body)) orelse return error.PeerCertificateMissing;
        const leaf = pki.parseCertificate(leaf_der) catch return error.PeerCertificateUntrusted;
        pki.verifyLeaf(&leaf, &self.identity.ca, self.now_s) catch |e| return switch (e) {
            error.Untrusted => error.PeerCertificateUntrusted,
            error.Expired => error.PeerCertificateExpired,
            error.NotYetValid => error.PeerCertificateNotYetValid,
        };
        const peer_pub = pki.Ed25519.PublicKey.fromBytes(leaf.publicKey().*) catch return error.PeerCertificateUntrusted;
        const name = leaf.commonName();
        const name_len = @min(name.len, pki.NAME_MAX);
        @memcpy(self.conn.peer_name[0..name_len], name[0..name_len]);
        self.conn.peer_name_len = @intCast(name_len);
        // CertificateVerify: over the transcript BEFORE it.
        const th_before_cv = self.th();
        const cv_body = try self.expect(.certificate_verify);
        const sig_bytes = try parseCertificateVerify(cv_body);
        var content: [64 + 34 + HASH_BYTES]u8 = undefined;
        const signed = certificateVerifyContent(peer_role, th_before_cv, &content);
        const sig = pki.Ed25519.Signature.fromBytes(sig_bytes);
        sig.verify(signed, peer_pub) catch return error.PeerSignatureInvalid;
        // Finished: over the transcript BEFORE it.
        const th_before_fin = self.th();
        const fin_body = try self.expect(.finished);
        const expect_mac = finishedMac(traffic_secret, th_before_fin);
        if (fin_body.len != HASH_BYTES or !std.crypto.timing_safe.eql([HASH_BYTES]u8, expect_mac, fin_body[0..HASH_BYTES].*)) return error.PeerSignatureInvalid;
    }
};

pub const Options = struct {
    /// The handshake's read bound (the listener's `DIAL_TIMEOUT`).
    timeout_ms: u32 = session.DIAL_TIMEOUT_MS,
    /// The wall clock the peer's certificate is judged at; null = now.
    now_s: ?u64 = null,
};

fn randomBytes(io: Io, comptime n: usize) HandshakeError![n]u8 {
    var b: [n]u8 = undefined;
    io.randomSecure(&b) catch return error.Transport;
    return b;
}

fn wallS(opts: Options) u64 {
    return opts.now_s orelse clock.realtimeCoarseS();
}

/// The server side over an accepted socket: on success the `Conn` owns
/// the record layer and `conn.link()` is the session's byte stream. A
/// refusal sends a fatal alert (best effort) and returns its class; the
/// caller counts, logs and closes the socket.
pub fn serverHandshake(gpa: std.mem.Allocator, inner: session.Link, identity: *const Identity, io: Io, opts: Options) HandshakeError!*Conn {
    const conn = try gpa.create(Conn);
    errdefer gpa.destroy(conn);
    conn.* = .{ .inner = inner, .role = .server };
    const hs = try gpa.create(Handshake);
    defer {
        hs.deinit();
        gpa.destroy(hs);
    }
    hs.* = .{ .conn = conn, .identity = identity, .io = io, .now_s = wallS(opts), .timeout_ms = opts.timeout_ms };
    serverBody(hs) catch |e| {
        switch (e) {
            error.PeerPlaintext, error.PeerNotTls, error.Eof, error.Timeout, error.Transport, error.Alert, error.OutOfMemory => {},
            error.ProfileMismatch => conn.writeAlert(.fatal, .handshake_failure),
            error.PeerCertificateMissing => conn.writeAlert(.fatal, .certificate_required),
            error.PeerCertificateUntrusted => conn.writeAlert(.fatal, .unknown_ca),
            error.PeerCertificateExpired, error.PeerCertificateNotYetValid => conn.writeAlert(.fatal, .certificate_expired),
            error.PeerSignatureInvalid => conn.writeAlert(.fatal, .decrypt_error),
            error.Protocol => conn.writeAlert(.fatal, .unexpected_message),
        }
        return e;
    };
    return conn;
}

fn serverBody(hs: *Handshake) HandshakeError!void {
    const conn = hs.conn;
    // 1. ClientHello (cleartext); a plaintext peer's frame magic where the
    //    record header should be is `PeerPlaintext` (the record reader).
    const ch_body = try hs.expect(.client_hello);
    const ch = try parseClientHello(ch_body);
    if (!ch.offers_version or !ch.offers_suite or !ch.offers_group or !ch.offers_scheme or !ch.offers_null_compression or ch.key_share == null) return error.ProfileMismatch;
    // 2. ServerHello.
    hs.ecdh = X25519.KeyPair.generate(hs.io);
    const server_random = try randomBytes(hs.io, 32);
    var flight_buf: [HANDSHAKE_BUF]u8 = undefined;
    var o: Out = .{ .buf = &flight_buf };
    try o.byte(@backingInt(tls.HandshakeType.server_hello));
    const sh_mark = try o.open24();
    try o.word(@backingInt(LEGACY_VERSION));
    try o.bytes(&server_random);
    try o.byte(@intCast(ch.session_id.len));
    try o.bytes(ch.session_id);
    try o.word(@backingInt(SUITE));
    try o.byte(@backingInt(tls.CompressionMethod.null));
    const ext_mark = try o.open16();
    try o.word(@backingInt(tls.ExtensionType.supported_versions));
    try o.word(2);
    try o.word(@backingInt(VERSION));
    try o.word(@backingInt(tls.ExtensionType.key_share));
    try o.word(2 + 2 + X25519.public_length);
    try o.word(@backingInt(GROUP));
    try o.word(X25519.public_length);
    try o.bytes(&hs.ecdh.public_key);
    o.close16(ext_mark);
    o.close24(sh_mark);
    hs.transcript.update(o.written());
    try hs.sendFlight(o.written());
    // 3. The handshake keys: the server writes under its own, reads the
    //    client's.
    try hs.deriveHandshakeSecrets(ch.key_share.?);
    conn.wr = .fromSecret(hs.server_hs);
    conn.rd = .fromSecret(hs.client_hs);
    // 4. EncryptedExtensions ‖ CertificateRequest ‖ Certificate ‖
    //    CertificateVerify ‖ Finished: one record.
    o = .{ .buf = &flight_buf };
    try o.byte(@backingInt(tls.HandshakeType.encrypted_extensions));
    try o.word24(2);
    try o.word(0);
    const ee_end = o.n;
    hs.transcript.update(o.buf[0..ee_end]);
    try o.byte(@backingInt(tls.HandshakeType.certificate_request));
    const cr_mark = try o.open24();
    try o.byte(0); // certificate_request_context: empty
    const cr_ext_mark = try o.open16();
    try o.word(@backingInt(tls.ExtensionType.signature_algorithms));
    try o.word(2 + 2);
    try o.word(2);
    try o.word(@backingInt(SCHEME));
    o.close16(cr_ext_mark);
    o.close24(cr_mark);
    hs.transcript.update(o.buf[ee_end..o.n]);
    try hs.buildAuthFlight(&o, hs.server_hs);
    try hs.sendFlight(o.written());
    // 5. The application secrets (the transcript through our Finished);
    //    our writes switch now, our reads after the client's Finished.
    const app = hs.deriveApplicationSecrets();
    conn.wr.?.zeroize();
    conn.wr = .fromSecret(app.server);
    // 6. The client's Certificate ‖ CertificateVerify ‖ Finished.
    try hs.readAuthFlight(.client, hs.client_hs);
    conn.rd.?.zeroize();
    conn.rd = .fromSecret(app.client);
    conn.established = true;
    stats.cluster_wire_tls_handshakes.inc();
}

/// The client side over a connected socket.
pub fn clientHandshake(gpa: std.mem.Allocator, inner: session.Link, identity: *const Identity, io: Io, opts: Options) HandshakeError!*Conn {
    const conn = try gpa.create(Conn);
    errdefer gpa.destroy(conn);
    conn.* = .{ .inner = inner, .role = .client };
    const hs = try gpa.create(Handshake);
    defer {
        hs.deinit();
        gpa.destroy(hs);
    }
    hs.* = .{ .conn = conn, .identity = identity, .io = io, .now_s = wallS(opts), .timeout_ms = opts.timeout_ms };
    clientBody(hs) catch |e| {
        switch (e) {
            error.PeerPlaintext, error.PeerNotTls, error.Eof, error.Timeout, error.Transport, error.Alert, error.OutOfMemory => {},
            error.ProfileMismatch => conn.writeAlert(.fatal, .handshake_failure),
            error.PeerCertificateMissing => conn.writeAlert(.fatal, .certificate_required),
            error.PeerCertificateUntrusted => conn.writeAlert(.fatal, .unknown_ca),
            error.PeerCertificateExpired, error.PeerCertificateNotYetValid => conn.writeAlert(.fatal, .certificate_expired),
            error.PeerSignatureInvalid => conn.writeAlert(.fatal, .decrypt_error),
            error.Protocol => conn.writeAlert(.fatal, .unexpected_message),
        }
        return e;
    };
    return conn;
}

fn clientBody(hs: *Handshake) HandshakeError!void {
    const conn = hs.conn;
    // 1. ClientHello: the profile, offered whole.
    hs.ecdh = X25519.KeyPair.generate(hs.io);
    const client_random = try randomBytes(hs.io, 32);
    var flight_buf: [HANDSHAKE_BUF]u8 = undefined;
    var o: Out = .{ .buf = &flight_buf };
    try o.byte(@backingInt(tls.HandshakeType.client_hello));
    const ch_mark = try o.open24();
    try o.word(@backingInt(LEGACY_VERSION));
    try o.bytes(&client_random);
    try o.byte(0); // legacy_session_id: empty (no middlebox compatibility)
    try o.word(2);
    try o.word(@backingInt(SUITE));
    try o.byte(1);
    try o.byte(@backingInt(tls.CompressionMethod.null));
    const ext_mark = try o.open16();
    try o.word(@backingInt(tls.ExtensionType.supported_versions));
    try o.word(1 + 2);
    try o.byte(2);
    try o.word(@backingInt(VERSION));
    try o.word(@backingInt(tls.ExtensionType.supported_groups));
    try o.word(2 + 2);
    try o.word(2);
    try o.word(@backingInt(GROUP));
    try o.word(@backingInt(tls.ExtensionType.signature_algorithms));
    try o.word(2 + 2);
    try o.word(2);
    try o.word(@backingInt(SCHEME));
    try o.word(@backingInt(tls.ExtensionType.key_share));
    try o.word(2 + 2 + 2 + X25519.public_length);
    try o.word(2 + 2 + X25519.public_length);
    try o.word(@backingInt(GROUP));
    try o.word(X25519.public_length);
    try o.bytes(&hs.ecdh.public_key);
    o.close16(ext_mark);
    o.close24(ch_mark);
    hs.transcript.update(o.written());
    try hs.sendFlight(o.written());
    // 2. ServerHello. A plaintext listener answers with its Challenge
    //    frame (the frame magic where a record header should be:
    //    `PeerPlaintext` from the record reader).
    const sh_body = try hs.expect(.server_hello);
    const sh = try parseServerHello(sh_body);
    if (std.mem.eql(u8, &sh.random, &tls.hello_retry_request_sequence)) return error.ProfileMismatch;
    if (sh.selected_version != @backingInt(VERSION) or sh.suite != @backingInt(SUITE) or sh.key_share == null) return error.ProfileMismatch;
    try hs.deriveHandshakeSecrets(sh.key_share.?);
    conn.rd = .fromSecret(hs.server_hs);
    conn.wr = .fromSecret(hs.client_hs);
    // 3. EncryptedExtensions, CertificateRequest (REQUIRED: the class is
    //    mutual), the server's auth flight.
    _ = try hs.expect(.encrypted_extensions);
    const m = try hs.nextMessage();
    if (m.ht != @backingInt(tls.HandshakeType.certificate_request)) return error.PeerCertificateMissing;
    hs.transcript.update(m.raw);
    if (!try parseCertificateRequest(m.body)) return error.ProfileMismatch;
    try hs.readAuthFlight(.server, hs.server_hs);
    // 4. The application secrets; the server's writes switched at its
    //    Finished, so our reads switch now.
    const app = hs.deriveApplicationSecrets();
    conn.rd.?.zeroize();
    conn.rd = .fromSecret(app.server);
    // 5. Our auth flight under the client handshake key, then our writes
    //    switch.
    o = .{ .buf = &flight_buf };
    try hs.buildAuthFlight(&o, hs.client_hs);
    try hs.sendFlight(o.written());
    conn.wr.?.zeroize();
    conn.wr = .fromSecret(app.client);
    conn.established = true;
    stats.cluster_wire_tls_handshakes.inc();
}

/// The refusal class a handshake error counts under at the listener (the
/// four counters) and the text for the log.
pub fn refusalText(e: HandshakeError) []const u8 {
    return switch (e) {
        error.PeerPlaintext => "a Plaintext peer at a MutualTls channel (the peer's SQUEEZEFS_WIRE_CHANNEL is plaintext)",
        error.PeerNotTls => "not a TLS peer",
        error.Protocol => "malformed or out-of-order TLS handshake",
        error.ProfileMismatch => "no common TLS profile (TLS 1.3, AES-128-GCM-SHA256, x25519, ed25519)",
        error.PeerCertificateMissing => "the peer presented no certificate",
        error.PeerCertificateUntrusted => "the peer's certificate is not under this set's CA",
        error.PeerCertificateExpired => "the peer's certificate has expired",
        error.PeerCertificateNotYetValid => "the peer's certificate is not yet valid",
        error.PeerSignatureInvalid => "the peer's CertificateVerify/Finished did not verify",
        error.Alert => "the peer sent a fatal TLS alert",
        error.Transport => "transport failure during the TLS handshake",
        error.Timeout => "the TLS handshake timed out",
        error.Eof => "the peer closed during the TLS handshake",
        error.OutOfMemory => "out of memory",
    };
}

/// Count a listener-side refusal under its class.
pub fn countRefusal(e: HandshakeError) void {
    switch (e) {
        error.PeerPlaintext => stats.cluster_wire_plaintext_refused.inc(),
        error.PeerCertificateUntrusted, error.PeerCertificateMissing, error.PeerSignatureInvalid => stats.cluster_wire_tls_refused_untrusted.inc(),
        error.PeerCertificateExpired, error.PeerCertificateNotYetValid => stats.cluster_wire_tls_refused_expired.inc(),
        else => stats.cluster_wire_tls_refused_protocol.inc(),
    }
}

// ---------------------------------------------------------------------------
// The files and the posture (Linux: the key file's 0600 law rides
// `transform.linux_keys.readKeyFile`).
// ---------------------------------------------------------------------------

/// The default material paths; the three knobs override each.
pub const TLS_DIR = "/etc/squeezefs/tls";
pub const DEFAULT_CA_PATH = TLS_DIR ++ "/ca.pem";
pub const DEFAULT_CERT_PATH = TLS_DIR ++ "/node.pem";
pub const DEFAULT_KEY_PATH = TLS_DIR ++ "/node.key";

pub const Paths = struct {
    ca: []const u8,
    cert: []const u8,
    key: []const u8,

    pub fn fromEnv(env: *const std.process.Environ.Map) Paths {
        return .{
            .ca = knobs.present(env.get("SQUEEZEFS_WIRE_TLS_CA")) orelse DEFAULT_CA_PATH,
            .cert = knobs.present(env.get("SQUEEZEFS_WIRE_TLS_CERT")) orelse DEFAULT_CERT_PATH,
            .key = knobs.present(env.get("SQUEEZEFS_WIRE_TLS_KEY")) orelse DEFAULT_KEY_PATH,
        };
    }
};

/// The class the knob names (`plaintext` when absent).
pub fn channelFromEnv(env: *const std.process.Environ.Map) Channel {
    return Channel.parse(knobs.enumKnob("SQUEEZEFS_WIRE_CHANNEL", env)) orelse .plaintext;
}

pub const InstallError = error{
    /// `SQUEEZEFS_WIRE_CHANNEL=mutual_tls` with no readable CA: the one
    /// configuration that is REFUSED, never downgraded.
    CaMissing,
    CaMalformed,
    CertMissing,
    CertMalformed,
    KeyMissing,
    KeyMalformed,
    /// The key file broke the 0600 / owner / symlink law.
    KeyFileLaw,
    LeafNotUnderCa,
    LeafExpired,
    LeafNotYetValid,
    KeyMismatch,
    OutOfMemory,
};

pub const Installed = struct {
    identity: ?*Identity,
    channel: Channel,

    pub fn deinit(self: *Installed, gpa: std.mem.Allocator) void {
        if (self.identity) |id| {
            install(null);
            id.deinit(gpa);
            gpa.destroy(id);
        }
        self.* = undefined;
    }
};

pub const linux_files = if (is_linux) struct {
    const linux = std.os.linux;
    const transform = @import("../data/transform.zig");

    /// A public material file (the CA, the node certificate): opened
    /// `O_NOFOLLOW|O_CLOEXEC`, a regular file owned by the euid or root
    /// that nobody else may WRITE (`mode & 0o022 == 0`: a world-writable CA
    /// is a trust store anyone edits), ≤ 64 KiB.
    pub fn readTrustFile(path: []const u8, out: []u8) error{ Missing, Law, TooLarge }![]const u8 {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{path}, 0) catch return error.Missing;
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        switch (linux.errno(rc)) {
            .SUCCESS => {},
            .LOOP => return error.Law,
            else => return error.Missing,
        }
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var stx: linux.Statx = undefined;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true, .MODE = true, .UID = true, .SIZE = true }, &stx)) != .SUCCESS) return error.Missing;
        if (stx.mode & linux.S.IFMT != linux.S.IFREG) return error.Law;
        if (stx.mode & 0o022 != 0) return error.Law;
        const euid = linux.geteuid();
        if (stx.uid != euid and stx.uid != 0) return error.Law;
        if (stx.size > out.len) return error.TooLarge;
        var n: usize = 0;
        while (n < out.len) {
            const r = linux.read(fd, out[n..].ptr, out.len - n);
            switch (linux.errno(r)) {
                .SUCCESS => {},
                .INTR => continue,
                else => return error.Missing,
            }
            if (r == 0) break;
            n += r;
        }
        return out[0..n];
    }

    /// Load the three files into an identity under the laws.
    pub fn loadIdentity(gpa: std.mem.Allocator, paths: Paths, now_s: u64) InstallError!*Identity {
        var ca_text: [transform.MAX_KEY_FILE_BYTES]u8 = undefined;
        const ca_pem = readTrustFile(paths.ca, &ca_text) catch return error.CaMissing;
        var ca_der: [pki.CERT_MAX_DER]u8 = undefined;
        const ca = pki.parseCertificatePem(ca_pem, &ca_der) catch return error.CaMalformed;
        var cert_text: [transform.MAX_KEY_FILE_BYTES]u8 = undefined;
        const cert_pem = readTrustFile(paths.cert, &cert_text) catch return error.CertMissing;
        var cert_der: [pki.CERT_MAX_DER]u8 = undefined;
        const cert = pki.parseCertificatePem(cert_pem, &cert_der) catch return error.CertMalformed;
        var key_text: [transform.MAX_KEY_FILE_BYTES + 1]u8 = undefined;
        defer std.crypto.secureZero(u8, &key_text);
        const key_pem = transform.linux_keys.readKeyFile(paths.key, &key_text) catch |e| return switch (e) {
            error.KeyFileOpenFailed => error.KeyMissing,
            error.KeyMaterialTooShort, error.KeyFileTooLarge => error.KeyMalformed,
            else => error.KeyFileLaw,
        };
        const key = pki.parsePrivateKeyPem(key_pem) catch return error.KeyMalformed;
        const id = try gpa.create(Identity);
        errdefer gpa.destroy(id);
        id.* = Identity.init(gpa, ca.der, cert.der, key, now_s) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.CertificateMalformed, error.CertificateNotEd25519, error.CertificateTooLarge => error.CertMalformed,
            error.LeafNotUnderCa => error.LeafNotUnderCa,
            error.LeafExpired => error.LeafExpired,
            error.LeafNotYetValid => error.LeafNotYetValid,
            error.KeyMismatch => error.KeyMismatch,
        };
        return id;
    }

    /// THE install: read the class knob; under `mutual_tls` load the
    /// identity (every failure is a loud refusal naming the file and the
    /// law) and install it for the process; under `plaintext` install
    /// nothing. Called once at bring-up, before the listener binds and
    /// before the first dial.
    pub fn installFromEnv(gpa: std.mem.Allocator, env: *const std.process.Environ.Map) InstallError!Installed {
        const ch = channelFromEnv(env);
        if (ch == .plaintext) {
            install(null);
            return .{ .identity = null, .channel = .plaintext };
        }
        const id = try loadIdentity(gpa, Paths.fromEnv(env), clock.realtimeCoarseS());
        install(id);
        log.info("cluster wire: channel class mutual_tls — node '{s}' under CA '{s}' (leaf valid to {d})", .{ id.nodeName(), id.ca.commonName(), id.cert.notAfterS() });
        return .{ .identity = id, .channel = .mutual_tls };
    }
} else struct {};

/// The refusal text for the console (`squeezefs mount` / the dialing verbs).
pub fn writeInstallRefusal(w: *Io.Writer, e: InstallError, paths: Paths) Io.Writer.Error!void {
    switch (e) {
        error.CaMissing => try w.print("SQUEEZEFS_WIRE_CHANNEL=mutual_tls but the CA certificate {s} cannot be read (SQUEEZEFS_WIRE_TLS_CA; a regular file owned by you or root that nobody else may write) — a CA-less TLS configuration is refused, never downgraded", .{paths.ca}),
        error.CaMalformed => try w.print("the CA certificate {s} is not a PEM Ed25519 certificate", .{paths.ca}),
        error.CertMissing => try w.print("the node certificate {s} cannot be read (SQUEEZEFS_WIRE_TLS_CERT)", .{paths.cert}),
        error.CertMalformed => try w.print("the node certificate {s} is not a PEM Ed25519 certificate", .{paths.cert}),
        error.KeyMissing => try w.print("the node key {s} cannot be read (SQUEEZEFS_WIRE_TLS_KEY)", .{paths.key}),
        error.KeyMalformed => try w.print("the node key {s} is not a PEM PKCS#8 Ed25519 private key", .{paths.key}),
        error.KeyFileLaw => try w.print("the node key {s} breaks the key-file law (a regular file, mode 0600, owned by you or root, never a symlink)", .{paths.key}),
        error.LeafNotUnderCa => try w.print("the node certificate {s} is not issued by the CA {s}", .{ paths.cert, paths.ca }),
        error.LeafExpired => try w.print("the node certificate {s} has expired — re-issue it (`squeezefs key tls-issue`)", .{paths.cert}),
        error.LeafNotYetValid => try w.print("the node certificate {s} is not yet valid (check the clock)", .{paths.cert}),
        error.KeyMismatch => try w.print("the node key {s} does not match the node certificate {s}", .{ paths.key, paths.cert }),
        error.OutOfMemory => try w.writeAll("out of memory loading the TLS identity"),
    }
}

// ---------------------------------------------------------------------------

const testing = std.testing;

/// Two in-memory byte queues between two threads (the portable tests'
/// socket): blocking reads under an `Io.Condition`, EOF on shutdown.
pub const ThreadPipe = struct {
    const Queue = struct {
        mutex: Io.Mutex = .init,
        cond: Io.Condition = .init,
        data: std.ArrayList(u8) = .empty,
        head: usize = 0,
        closed: bool = false,
    };

    io: Io,
    gpa: std.mem.Allocator,
    a_to_b: Queue = .{},
    b_to_a: Queue = .{},
    /// The two ends live INSIDE the pipe (a `Link`'s ctx points at one).
    ends: [2]End = undefined,

    pub const End = struct {
        pipe: *ThreadPipe,
        rx: *Queue,
        tx: *Queue,

        pub fn link(self: *End) session.Link {
            return .{ .ctx = self, .vtable = &end_vtable };
        }
    };

    /// Place, then `arm` (the ends point into the pipe's own storage).
    pub fn init(io: Io, gpa: std.mem.Allocator) ThreadPipe {
        return .{ .io = io, .gpa = gpa };
    }

    pub fn arm(self: *ThreadPipe) void {
        self.ends[0] = .{ .pipe = self, .rx = &self.b_to_a, .tx = &self.a_to_b };
        self.ends[1] = .{ .pipe = self, .rx = &self.a_to_b, .tx = &self.b_to_a };
    }

    pub fn deinit(self: *ThreadPipe) void {
        self.a_to_b.data.deinit(self.gpa);
        self.b_to_a.data.deinit(self.gpa);
    }

    pub fn linkA(self: *ThreadPipe) session.Link {
        return self.ends[0].link();
    }
    pub fn linkB(self: *ThreadPipe) session.Link {
        return self.ends[1].link();
    }

    const end_vtable: session.Link.VTable = .{ .readExact = readExact, .writeAll = writeAll, .shutdown = shutdown, .close = close };

    fn readExact(ctx: *anyopaque, buf: []u8, timeout_ms: ?u32) session.FrameError!void {
        const self: *End = @ptrCast(@alignCast(ctx));
        const io = self.pipe.io;
        const q = self.rx;
        q.mutex.lockUncancelable(io);
        defer q.mutex.unlock(io);
        var filled: usize = 0;
        while (filled < buf.len) {
            const avail = q.data.items.len - q.head;
            if (avail == 0) {
                if (q.closed) return error.Eof;
                if (timeout_ms) |ms| {
                    q.cond.waitTimeout(io, &q.mutex, .{ .duration = .{ .raw = .fromMilliseconds(@intCast(ms)), .clock = .awake } }) catch return error.Timeout;
                } else q.cond.wait(io, &q.mutex) catch return error.TransportFailed;
                continue;
            }
            const n = @min(avail, buf.len - filled);
            @memcpy(buf[filled..][0..n], q.data.items[q.head..][0..n]);
            q.head += n;
            filled += n;
        }
    }

    fn writeAll(ctx: *anyopaque, bytes: []const u8) session.FrameError!void {
        const self: *End = @ptrCast(@alignCast(ctx));
        const io = self.pipe.io;
        const q = self.tx;
        q.mutex.lockUncancelable(io);
        defer q.mutex.unlock(io);
        if (q.closed) return error.TransportFailed;
        q.data.appendSlice(self.pipe.gpa, bytes) catch return error.TransportFailed;
        q.cond.broadcast(io);
    }

    fn closeQueue(io: Io, q: *Queue) void {
        q.mutex.lockUncancelable(io);
        defer q.mutex.unlock(io);
        q.closed = true;
        q.cond.broadcast(io);
    }

    fn shutdown(ctx: *anyopaque) void {
        const self: *End = @ptrCast(@alignCast(ctx));
        closeQueue(self.pipe.io, self.rx);
        closeQueue(self.pipe.io, self.tx);
    }

    fn close(ctx: *anyopaque) void {
        shutdown(ctx);
    }
};

/// A CA with two node identities under it (and a foreign CA's node).
pub const TestPki = struct {
    ca: pki.Issued,
    a: pki.Issued,
    b: pki.Issued,
    foreign_ca: pki.Issued,
    foreign: pki.Issued,
    now: u64,

    pub fn init(io: Io, now: u64) !TestPki {
        const ca = try pki.mintCa(io, "test-ca", now - 60);
        const foreign_ca = try pki.mintCa(io, "test-ca", now - 60);
        return .{
            .ca = ca,
            .a = try pki.mintNode(io, &ca, "test-ca", "node-a", now - 60, pki.NODE_VALIDITY_S),
            .b = try pki.mintNode(io, &ca, "test-ca", "node-b", now - 60, pki.NODE_VALIDITY_S),
            .foreign_ca = foreign_ca,
            .foreign = try pki.mintNode(io, &foreign_ca, "test-ca", "node-x", now - 60, pki.NODE_VALIDITY_S),
            .now = now,
        };
    }

    pub fn identity(self: *const TestPki, gpa: std.mem.Allocator, which: enum { a, b, foreign }) !Identity {
        return switch (which) {
            .a => Identity.init(gpa, self.ca.der(), self.a.der(), self.a.key, self.now),
            .b => Identity.init(gpa, self.ca.der(), self.b.der(), self.b.key, self.now),
            .foreign => Identity.init(gpa, self.foreign_ca.der(), self.foreign.der(), self.foreign.key, self.now),
        };
    }
};

const ServerRun = struct {
    gpa: std.mem.Allocator,
    io: Io,
    link: session.Link,
    identity: *const Identity,
    opts: Options = .{},
    conn: ?*Conn = null,
    err: ?HandshakeError = null,

    fn run(self: *ServerRun) void {
        self.conn = serverHandshake(self.gpa, self.link, self.identity, self.io, self.opts) catch |e| {
            self.err = e;
            return;
        };
    }
};

fn pair(gpa: std.mem.Allocator, io: Io, pipe: *ThreadPipe, server_id: *const Identity, client_id: *const Identity, opts: Options) !struct { server: *Conn, client: *Conn } {
    var sr: ServerRun = .{ .gpa = gpa, .io = io, .link = pipe.linkA(), .identity = server_id, .opts = opts };
    const t = try std.Thread.spawn(.{}, ServerRun.run, .{&sr});
    const client = clientHandshake(gpa, pipe.linkB(), client_id, io, opts) catch |e| {
        pipe.linkB().shutdown();
        t.join();
        if (sr.conn) |c| c.destroy(gpa);
        return e;
    };
    t.join();
    if (sr.err) |e| {
        client.destroy(gpa);
        return e;
    }
    return .{ .server = sr.conn.?, .client = client };
}

test "a mutual handshake under one CA: both sides established, the exporter agrees, the peer names are the leaves' CNs, records round-trip in both directions through the Link, a 1 MiB frame crosses in 64 records, KeyUpdate rotates and the stream continues" {
    const gpa = testing.allocator;
    const io = testing.io;
    const now: u64 = 1_760_000_000;
    const tp = try TestPki.init(io, now);
    var id_a = try tp.identity(gpa, .a);
    defer id_a.deinit(gpa);
    var id_b = try tp.identity(gpa, .b);
    defer id_b.deinit(gpa);
    var pipe = ThreadPipe.init(io, gpa);
    pipe.arm();
    defer pipe.deinit();
    const p = try pair(gpa, io, &pipe, &id_a, &id_b, .{ .now_s = now });
    defer p.server.destroy(gpa);
    defer p.client.destroy(gpa);
    try testing.expect(p.server.established and p.client.established);
    try testing.expectEqualStrings("node-b", p.server.peerName());
    try testing.expectEqualStrings("node-a", p.client.peerName());
    try testing.expectEqual(p.server.binding(), p.client.binding());
    var other: [BINDING_BYTES]u8 = undefined;
    p.server.exporter("another label", &other);
    try testing.expect(!std.mem.eql(u8, &other, &p.server.binding()));
    // Records both ways.
    var sl = p.server.link();
    var cl = p.client.link();
    try cl.writeAll("hello from the client");
    var buf: [64]u8 = undefined;
    try sl.readExact(buf[0..21], 1000);
    try testing.expectEqualStrings("hello from the client", buf[0..21]);
    try sl.writeAll("and back");
    try cl.readExact(buf[0..8], 1000);
    try testing.expectEqualStrings("and back", buf[0..8]);
    // A frame wider than a record.
    const big = try gpa.alloc(u8, (1 << 20) + 80);
    defer gpa.free(big);
    for (big, 0..) |*x, i| x.* = @truncate(i *% 31);
    const seq_before = p.client.wr.?.seq;
    try cl.writeAll(big);
    try testing.expectEqual(seq_before + 65, p.client.wr.?.seq);
    const got = try gpa.alloc(u8, big.len);
    defer gpa.free(got);
    try sl.readExact(got, 1000);
    try testing.expectEqualSlices(u8, big, got);
    // KeyUpdate from the server side: the client's reads follow.
    try p.server.sendKeyUpdate();
    try sl.writeAll("after the update");
    try cl.readExact(buf[0..16], 1000);
    try testing.expectEqualStrings("after the update", buf[0..16]);
    try testing.expectEqual(@as(u32, 1), p.client.key_updates_received);
    // A peer-requested update: the client's next write rotates first.
    const req = [_]u8{ @backingInt(tls.HandshakeType.key_update), 0, 0, 1, @backingInt(tls.KeyUpdateRequest.update_requested) };
    try p.server.writeRecord(.handshake, &req);
    var next_s = p.server.wr.?.updated();
    p.server.wr = next_s;
    next_s.zeroize();
    try sl.writeAll("x");
    try cl.readExact(buf[0..1], 1000);
    try testing.expect(p.client.peer_update_requested.load(.acquire));
    try cl.writeAll("rotated");
    try testing.expectEqual(@as(u32, 1), p.client.key_updates_sent);
    try sl.readExact(buf[0..7], 1000);
    try testing.expectEqualStrings("rotated", buf[0..7]);
    // close_notify is an orderly EOF.
    cl.close();
    try testing.expectError(error.Eof, sl.readExact(buf[0..1], 1000));
}

test "refusals: a client under a foreign CA (same issuer name, another key) is Untrusted; an expired leaf is Expired; a not-yet-valid leaf is NotYetValid; a server under a foreign CA is refused by the client; a plaintext frame at the server is PeerPlaintext; a Challenge frame at the client is PeerPlaintext; garbage is PeerNotTls" {
    const gpa = testing.allocator;
    const io = testing.io;
    const now: u64 = 1_760_000_000;
    const tp = try TestPki.init(io, now);
    var id_a = try tp.identity(gpa, .a);
    defer id_a.deinit(gpa);
    var id_b = try tp.identity(gpa, .b);
    defer id_b.deinit(gpa);
    var id_x = try tp.identity(gpa, .foreign);
    defer id_x.deinit(gpa);
    {
        var pipe = ThreadPipe.init(io, gpa);
        pipe.arm();
        defer pipe.deinit();
        var sr: ServerRun = .{ .gpa = gpa, .io = io, .link = pipe.linkA(), .identity = &id_a, .opts = .{ .now_s = now } };
        const t = try std.Thread.spawn(.{}, ServerRun.run, .{&sr});
        // The client under the foreign CA refuses the server FIRST (its
        // leaf is not under the foreign CA) and alerts.
        try testing.expectError(error.PeerCertificateUntrusted, clientHandshake(gpa, pipe.linkB(), &id_x, io, .{ .now_s = now }));
        pipe.linkB().shutdown();
        t.join();
        try testing.expect(sr.conn == null);
    }
    {
        // The server judges a leaf that expired: the client's clock is
        // the same, but the server is the refusing side here because the
        // client's identity is valid at ITS own time (`now_s` later).
        var pipe = ThreadPipe.init(io, gpa);
        pipe.arm();
        defer pipe.deinit();
        var sr: ServerRun = .{ .gpa = gpa, .io = io, .link = pipe.linkA(), .identity = &id_a, .opts = .{ .now_s = now + pki.NODE_VALIDITY_S + 1 } };
        const t = try std.Thread.spawn(.{}, ServerRun.run, .{&sr});
        // The client sees the server's leaf as expired too at that clock,
        // so give the client the valid clock: it accepts the server, sends
        // its flight, and the server refuses it as Expired.
        const r = clientHandshake(gpa, pipe.linkB(), &id_b, io, .{ .now_s = now });
        if (r) |c| c.destroy(gpa) else |_| {}
        t.join();
        try testing.expectEqual(@as(?HandshakeError, error.PeerCertificateExpired), sr.err);
    }
    {
        var pipe = ThreadPipe.init(io, gpa);
        pipe.arm();
        defer pipe.deinit();
        var sr: ServerRun = .{ .gpa = gpa, .io = io, .link = pipe.linkA(), .identity = &id_a, .opts = .{ .now_s = now - 3600 } };
        const t = try std.Thread.spawn(.{}, ServerRun.run, .{&sr});
        const r = clientHandshake(gpa, pipe.linkB(), &id_b, io, .{ .now_s = now });
        if (r) |c| c.destroy(gpa) else |_| {}
        t.join();
        try testing.expectEqual(@as(?HandshakeError, error.PeerCertificateNotYetValid), sr.err);
    }
    {
        // A server under the foreign CA: the honest client refuses it.
        var pipe = ThreadPipe.init(io, gpa);
        pipe.arm();
        defer pipe.deinit();
        var sr: ServerRun = .{ .gpa = gpa, .io = io, .link = pipe.linkA(), .identity = &id_x, .opts = .{ .now_s = now } };
        const t = try std.Thread.spawn(.{}, ServerRun.run, .{&sr});
        try testing.expectError(error.PeerCertificateUntrusted, clientHandshake(gpa, pipe.linkB(), &id_b, io, .{ .now_s = now }));
        pipe.linkB().shutdown();
        t.join();
    }
    {
        // A plaintext frame header where the ClientHello should be.
        var pipe = ThreadPipe.init(io, gpa);
        pipe.arm();
        defer pipe.deinit();
        var hb: [frame.HEADER_LEN]u8 = undefined;
        frame.encodeHeader(.{ .flags = frame.FLAG_DIR_PEER_TO_COORD, .len = 0, .verb = 0xFFF1, .status = 0, .correlation = 0, .seq = 0, .schema = 6 }, "", &hb);
        try pipe.linkB().writeAll(&hb);
        try testing.expectError(error.PeerPlaintext, serverHandshake(gpa, pipe.linkA(), &id_a, io, .{ .now_s = now }));
        // A Challenge frame where the ServerHello should be.
        var pipe2 = ThreadPipe.init(io, gpa);
        pipe2.arm();
        defer pipe2.deinit();
        try pipe2.linkA().writeAll(&hb);
        try testing.expectError(error.PeerPlaintext, clientHandshake(gpa, pipe2.linkB(), &id_b, io, .{ .now_s = now }));
        // Garbage.
        var pipe3 = ThreadPipe.init(io, gpa);
        pipe3.arm();
        defer pipe3.deinit();
        try pipe3.linkB().writeAll("GET / HTTP/1.0\r\n\r\n");
        try testing.expectError(error.PeerNotTls, serverHandshake(gpa, pipe3.linkA(), &id_a, io, .{ .now_s = now }));
    }
}

test "the identity refuses its own bad material: a leaf under another CA, a key that is not the leaf's, an expired leaf" {
    const gpa = testing.allocator;
    const io = testing.io;
    const now: u64 = 1_760_000_000;
    const tp = try TestPki.init(io, now);
    try testing.expectError(error.LeafNotUnderCa, Identity.init(gpa, tp.foreign_ca.der(), tp.a.der(), tp.a.key, now));
    try testing.expectError(error.KeyMismatch, Identity.init(gpa, tp.ca.der(), tp.a.der(), tp.b.key, now));
    try testing.expectError(error.LeafExpired, Identity.init(gpa, tp.ca.der(), tp.a.der(), tp.a.key, now + pki.NODE_VALIDITY_S + 1));
    try testing.expectError(error.CertificateMalformed, Identity.init(gpa, "nope", tp.a.der(), tp.a.key, now));
    // The posture: nothing installed = plaintext; an identity = mutual_tls.
    try testing.expectEqual(Channel.plaintext, posture().channel());
    var id = try tp.identity(gpa, .a);
    defer id.deinit(gpa);
    install(&id);
    try testing.expectEqual(Channel.mutual_tls, posture().channel());
    try testing.expectEqual(@as(u64, 1), stats.cluster_wire_channel_mutual_tls.load());
    install(null);
    try testing.expectEqual(Channel.plaintext, posture().channel());
    try testing.expectEqual(@as(?Channel, .mutual_tls), Channel.parse("mutual_tls"));
    try testing.expectEqual(@as(?Channel, null), Channel.parse("tls"));
    try testing.expect(Channel.mutual_tls.confidential() and !Channel.plaintext.confidential());
}

test "the handshake decoders: a ClientHello with the profile offers everything; one without x25519 offers no group; a ServerHello round-trips; a Certificate message yields the leaf; an empty list is null" {
    // Build a ClientHello body the way `clientBody` does and parse it back.
    var buf: [512]u8 = undefined;
    var o: Out = .{ .buf = &buf };
    try o.word(@backingInt(LEGACY_VERSION));
    const rnd: [32]u8 = @splat(7);
    try o.bytes(&rnd);
    try o.byte(0);
    try o.word(2);
    try o.word(@backingInt(SUITE));
    try o.byte(1);
    try o.byte(0);
    const em = try o.open16();
    try o.word(@backingInt(tls.ExtensionType.supported_versions));
    try o.word(3);
    try o.byte(2);
    try o.word(@backingInt(VERSION));
    try o.word(@backingInt(tls.ExtensionType.supported_groups));
    try o.word(4);
    try o.word(2);
    try o.word(@backingInt(GROUP));
    try o.word(@backingInt(tls.ExtensionType.signature_algorithms));
    try o.word(4);
    try o.word(2);
    try o.word(@backingInt(SCHEME));
    try o.word(@backingInt(tls.ExtensionType.key_share));
    try o.word(38);
    try o.word(36);
    try o.word(@backingInt(GROUP));
    try o.word(32);
    const share: [32]u8 = @splat(9);
    try o.bytes(&share);
    o.close16(em);
    const ch = try parseClientHello(o.written());
    try testing.expect(ch.offers_version and ch.offers_suite and ch.offers_group and ch.offers_scheme and ch.offers_null_compression);
    try testing.expectEqual(share, ch.key_share.?);
    try testing.expectEqual(rnd, ch.random);
    // Truncated: Protocol.
    try testing.expectError(error.Protocol, parseClientHello(o.written()[0 .. o.n - 1]));
    // A certificate message.
    var cb: [64]u8 = undefined;
    var co: Out = .{ .buf = &cb };
    try co.byte(0);
    try co.word24(3 + 5 + 2);
    try co.word24(5);
    try co.bytes("ABCDE");
    try co.word(0);
    try testing.expectEqualStrings("ABCDE", (try parseCertificateMsg(co.written())).?);
    try testing.expectEqual(@as(?[]const u8, null), try parseCertificateMsg(&.{ 0, 0, 0, 0 }));
    try testing.expect(try parseCertificateRequest(&.{ 0, 0, 8, 0, 0x0D, 0, 4, 0, 2, 0x08, 0x07 }));
    try testing.expect(!try parseCertificateRequest(&.{ 0, 0, 0 }));
}

fn tlsFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [1024]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseClientHello(buf[0..len]) catch {};
    _ = parseServerHello(buf[0..len]) catch {};
    _ = parseCertificateMsg(buf[0..len]) catch {};
    _ = parseCertificateVerify(buf[0..len]) catch {};
    _ = parseCertificateRequest(buf[0..len]) catch {};
    _ = Channel.parse(buf[0..len]);
}

test "fuzz: the TLS handshake decoders are total" {
    try fuzzkit.fuzz({}, tlsFuzz, .{});
}
