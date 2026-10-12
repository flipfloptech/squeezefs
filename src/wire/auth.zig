//! The storage-trust ladder (charter §8 "HMAC enrollment against a
//! `job:enroll` secret"; decision 0003 §E; `docs/porting/sym_wire_
//! membership_recovery.md` §1.3): whoever can READ the set's `job:enroll`
//! record is inside the trust domain, so possession of its 32 B secret is
//! the credential every cluster-wire peer proves.
//!
//! The record: `JobEnrollV1` on tree 0 of volume 0 beside `set_config`,
//! minted at format from `Io.randomSecure` (never derived from anything
//! an attacker could read). A present-but-undecodable record REFUSES
//! loud and is never overwritten — the rotation lever is removing it on a
//! quiesced set (a transient read error must never re-mint, or every
//! peer's proofs go invalid at once).
//!
//! The ladder, ONE implementation (`Gate.verify`, order load-bearing):
//!   1. the coordinator speaks FIRST: `Challenge{server_nonce, freshness}`
//!      — a SERVER-issued single-use nonce inside a freshness window, so
//!      a captured hello is not a credential;
//!   2. the peer answers `Prove{peer_id, server_nonce, peer_nonce, mac}`
//!      with `mac = HMAC-SHA256(secret, peer_id ‖ server_nonce ‖
//!      peer_nonce ‖ "hello")`;
//!   3. verify: schema → peer-id bounds → constant-time MAC compare →
//!      ONLY THEN consume the nonce (Fresh / Expired / UnknownOrReplayed)
//!      — a peer that cannot produce a valid MAC never spends an honest
//!      connection's nonce;
//!   4. both sides derive the session key (below); 5. `Admitted` → every
//!      later frame carries the per-frame MAC (`frame.zig`).
//!
//! Session key (HKDF-SHA256; the labels are THIS port's and recorded in
//! decision 0002 row 226):
//!     prk = HKDF-Extract(salt = server_nonce ‖ peer_nonce, ikm = secret)
//!     key = HKDF-Expand(prk, info = "squeezefs-cluster-wire/v1/session" ‖ 0x00
//!                              ‖ peer_id ‖ 0x00 ‖ binding, 32)
//! `binding` is the channel's exporter — empty on plaintext; on mTLS
//! (rung 8) the RFC 5705 exporter under "squeezefs-cluster-wire/v1/binding"
//! (32 B) — so a session key is bound to the secret, both nonces, the peer
//! and the channel, and rung 8 adds TLS without a wire change.
//!
//! Portable; the randomness comes through the `Io` the caller passes.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const Io = std.Io;
const checksum = @import("../core/checksum.zig");
const verbs = @import("verbs.zig");
const frame = @import("frame.zig");

pub const Hmac = std.crypto.auth.hmac.sha2.HmacSha256;
pub const Hkdf = std.crypto.kdf.hkdf.HkdfSha256;

pub const SECRET_BYTES: usize = 32;
pub const Secret = [SECRET_BYTES]u8;
pub const Nonce = [verbs.NONCE_BYTES]u8;
pub const Proof = [verbs.PROOF_BYTES]u8;
pub const SessionKey = frame.SessionKey;

pub const HELLO_LABEL = "hello";
pub const SESSION_LABEL = "squeezefs-cluster-wire/v1/session";
pub const BINDING_LABEL = "squeezefs-cluster-wire/v1/binding";
pub const BINDING_BYTES: usize = 32;

/// The challenge window: a captured hello is dead after it. 30 s covers
/// the slowest honest dial (DIAL_TIMEOUT 10 s) three times over.
pub const ENROLL_FRESHNESS_MS: u64 = 30_000;

/// The nonce registry's cap: every live connection may hold one open
/// challenge and a dial storm overshoots by the accept backlog; 4× the
/// connection cap with a floor of 64 (the shipped posture).
pub fn nonceCap(max_connections: u32) u32 {
    return @max(4 * max_connections, 64);
}

// ---------------------------------------------------------------------------
// The `job:enroll` record.
// ---------------------------------------------------------------------------

pub const ENROLL_KEY: []const u8 = "job:enroll";
pub const ENROLL_MAGIC: [4]u8 = "SQZE".*;
pub const ENROLL_VERSION: u16 = 1;

pub const JobEnrollV1 = extern struct {
    magic: [4]u8,
    version_le: [2]u8,
    reserved_le: [2]u8,
    secret: Secret,
    csum_le: [8]u8,
};

pub const ENROLL_LEN: usize = @sizeOf(JobEnrollV1);

comptime {
    std.debug.assert(ENROLL_LEN == 48);
    std.debug.assert(@offsetOf(JobEnrollV1, "secret") == 8);
    std.debug.assert(@offsetOf(JobEnrollV1, "csum_le") == 40);
}

/// Mint a fresh secret (format time; `Io.randomSecure` is the one source
/// of key material, decision 0002 row 119).
pub fn mintSecret(io: Io) Io.RandomSecureError!Secret {
    var s: Secret = undefined;
    try io.randomSecure(&s);
    return s;
}

pub fn encodeEnroll(secret: Secret) [ENROLL_LEN]u8 {
    var w: JobEnrollV1 = undefined;
    w.magic = ENROLL_MAGIC;
    std.mem.writeInt(u16, &w.version_le, ENROLL_VERSION, .little);
    w.reserved_le = @splat(0);
    w.secret = secret;
    w.csum_le = @splat(0);
    var bytes = std.mem.toBytes(w);
    checksum.stamp(&bytes, @offsetOf(JobEnrollV1, "csum_le"));
    return bytes;
}

/// Total: exactly the encoder's form decodes; anything else is `Corrupt`
/// (the record stands, the mount refuses loud — never re-minted).
pub fn decodeEnroll(bytes: []const u8) error{Corrupt}!Secret {
    if (bytes.len != ENROLL_LEN) return error.Corrupt;
    if (!checksum.verify(bytes, @offsetOf(JobEnrollV1, "csum_le"))) return error.Corrupt;
    const w = std.mem.bytesToValue(JobEnrollV1, bytes[0..ENROLL_LEN]);
    if (!std.mem.eql(u8, &w.magic, &ENROLL_MAGIC)) return error.Corrupt;
    if (std.mem.readInt(u16, &w.version_le, .little) != ENROLL_VERSION) return error.Corrupt;
    if (!std.mem.allEqual(u8, &w.reserved_le, 0)) return error.Corrupt;
    return w.secret;
}

// ---------------------------------------------------------------------------
// The proof and the session key.
// ---------------------------------------------------------------------------

/// `HMAC-SHA256(secret, peer_id ‖ server_nonce ‖ peer_nonce ‖ "hello")`.
pub fn proof(secret: *const Secret, peer_id: []const u8, server_nonce: *const Nonce, peer_nonce: *const Nonce) Proof {
    var h = Hmac.init(secret);
    h.update(peer_id);
    h.update(server_nonce);
    h.update(peer_nonce);
    h.update(HELLO_LABEL);
    var out: Proof = undefined;
    h.final(&out);
    return out;
}

/// The module doc's derivation; `binding` empty on plaintext.
pub fn sessionKey(secret: *const Secret, peer_id: []const u8, server_nonce: *const Nonce, peer_nonce: *const Nonce, binding: []const u8) SessionKey {
    var salt: [2 * verbs.NONCE_BYTES]u8 = undefined;
    @memcpy(salt[0..verbs.NONCE_BYTES], server_nonce);
    @memcpy(salt[verbs.NONCE_BYTES..], peer_nonce);
    const prk = Hkdf.extract(&salt, secret);
    var info: [SESSION_LABEL.len + 1 + verbs.PEER_ID_MAX + 1 + BINDING_BYTES]u8 = undefined;
    var n: usize = 0;
    @memcpy(info[n..][0..SESSION_LABEL.len], SESSION_LABEL);
    n += SESSION_LABEL.len;
    info[n] = 0;
    n += 1;
    std.debug.assert(peer_id.len <= verbs.PEER_ID_MAX and binding.len <= BINDING_BYTES);
    @memcpy(info[n..][0..peer_id.len], peer_id);
    n += peer_id.len;
    info[n] = 0;
    n += 1;
    @memcpy(info[n..][0..binding.len], binding);
    n += binding.len;
    var key: SessionKey = undefined;
    Hkdf.expand(&key, info[0..n], prk);
    return key;
}

// ---------------------------------------------------------------------------
// The server-issued nonce registry.
// ---------------------------------------------------------------------------

pub const NonceVerdict = enum { fresh, expired, unknown_or_replayed };

/// Bounded by `nonceCap`, pruned by age and cap; a consumed nonce is
/// gone (single use). Fixed storage allocated once at the listener's
/// start; one spin lock (the handshake is never a hot path).
pub const NonceRegistry = struct {
    const Entry = struct { nonce: Nonce, issued_ms: u64, live: bool };

    entries: []Entry,
    lock: std.atomic.Value(bool) = .init(false),
    /// `cluster_wire_outstanding_challenges`'s source.
    outstanding: u32 = 0,

    pub fn init(gpa: std.mem.Allocator, cap: u32) std.mem.Allocator.Error!NonceRegistry {
        const e = try gpa.alloc(Entry, cap);
        for (e) |*x| x.* = .{ .nonce = @splat(0), .issued_ms = 0, .live = false };
        return .{ .entries = e };
    }

    pub fn deinit(self: *NonceRegistry, gpa: std.mem.Allocator) void {
        gpa.free(self.entries);
    }

    fn acquire(self: *NonceRegistry) void {
        while (self.lock.swap(true, .acquire)) std.atomic.spinLoopHint();
    }
    fn release(self: *NonceRegistry) void {
        self.lock.store(false, .release);
    }

    fn pruneExpired(self: *NonceRegistry, now_ms: u64) void {
        for (self.entries) |*e| if (e.live and now_ms -| e.issued_ms > ENROLL_FRESHNESS_MS) {
            e.live = false;
            self.outstanding -= 1;
        };
    }

    /// Register a fresh nonce; past the cap the OLDEST live one is evicted
    /// (a dial storm past the cap costs the stalest straggler its window,
    /// never the registry its bound).
    pub fn issue(self: *NonceRegistry, nonce: Nonce, now_ms: u64) void {
        self.acquire();
        defer self.release();
        self.pruneExpired(now_ms);
        var slot: ?*Entry = null;
        var oldest: ?*Entry = null;
        for (self.entries) |*e| {
            if (!e.live) {
                slot = e;
                break;
            }
            if (oldest == null or e.issued_ms < oldest.?.issued_ms) oldest = e;
        }
        const target = slot orelse blk: {
            self.outstanding -= 1;
            break :blk oldest.?;
        };
        target.* = .{ .nonce = nonce, .issued_ms = now_ms, .live = true };
        self.outstanding += 1;
    }

    /// Consume: fresh (and gone), expired (and gone), or never issued /
    /// already spent.
    pub fn consume(self: *NonceRegistry, nonce: *const Nonce, now_ms: u64) NonceVerdict {
        self.acquire();
        defer self.release();
        for (self.entries) |*e| {
            if (!e.live or !std.mem.eql(u8, &e.nonce, nonce)) continue;
            e.live = false;
            self.outstanding -= 1;
            return if (now_ms -| e.issued_ms > ENROLL_FRESHNESS_MS) .expired else .fresh;
        }
        return .unknown_or_replayed;
    }

    pub fn liveCount(self: *NonceRegistry) u32 {
        self.acquire();
        defer self.release();
        return self.outstanding;
    }
};

// ---------------------------------------------------------------------------
// The coordinator's gate and the peer's answer.
// ---------------------------------------------------------------------------

pub const RefuseReason = enum {
    schema,
    peer_id_bounds,
    mac,
    nonce_expired,
    nonce_unknown_or_replayed,

    pub fn text(self: RefuseReason) []const u8 {
        return switch (self) {
            .schema => "wire schema not supported — refusing rather than interpreting a foreign schema",
            .peer_id_bounds => "peer id outside its bounds",
            .mac => "proof mac invalid (not enrolled against this set's job:enroll secret)",
            .nonce_expired => "challenge expired",
            .nonce_unknown_or_replayed => "challenge unknown or already spent",
        };
    }
};

pub const Admission = struct {
    session_key: SessionKey,
    peer_id_len: u8,
    peer_id: [verbs.PEER_ID_MAX]u8,

    pub fn peerId(self: *const Admission) []const u8 {
        return self.peer_id[0..self.peer_id_len];
    }

    /// A production appender's peer id IS its member id.
    pub fn member(self: *const Admission) ?verbs.WireIdentity {
        return verbs.WireIdentity.fromPeerId(self.peerId());
    }
};

pub const Verdict = union(enum) { admitted: Admission, refused: RefuseReason };

/// The coordinator side: owns the secret and the nonce registry; `binding`
/// is the channel's exporter (empty on plaintext).
pub const Gate = struct {
    secret: Secret,
    nonces: NonceRegistry,

    pub fn init(gpa: std.mem.Allocator, secret: Secret, max_connections: u32) std.mem.Allocator.Error!Gate {
        return .{ .secret = secret, .nonces = try NonceRegistry.init(gpa, nonceCap(max_connections)) };
    }

    pub fn deinit(self: *Gate, gpa: std.mem.Allocator) void {
        self.nonces.deinit(gpa);
        std.crypto.secureZero(u8, &self.secret);
    }

    /// Step 1: a fresh challenge (the nonce from the `Io`'s secure source).
    pub fn challenge(self: *Gate, io: Io, now_ms: u64) Io.RandomSecureError!verbs.Challenge {
        var nonce: Nonce = undefined;
        try io.randomSecure(&nonce);
        self.nonces.issue(nonce, now_ms);
        return .{ .schema = verbs.CLUSTER_WIRE_SCHEMA, .freshness_ms = @intCast(ENROLL_FRESHNESS_MS), .server_nonce = nonce };
    }

    /// Step 3, in the load-bearing order.
    pub fn verify(self: *Gate, prove: verbs.Prove, binding: []const u8, now_ms: u64) Verdict {
        if (prove.head.schema != verbs.CLUSTER_WIRE_SCHEMA) return .{ .refused = .schema };
        const peer_id = prove.bytes;
        if (peer_id.len == 0 or peer_id.len > verbs.PEER_ID_MAX) return .{ .refused = .peer_id_bounds };
        const expect = proof(&self.secret, peer_id, &prove.head.server_nonce, &prove.head.peer_nonce);
        if (!std.crypto.timing_safe.eql(Proof, expect, prove.head.mac)) return .{ .refused = .mac };
        switch (self.nonces.consume(&prove.head.server_nonce, now_ms)) {
            .fresh => {},
            .expired => return .{ .refused = .nonce_expired },
            .unknown_or_replayed => return .{ .refused = .nonce_unknown_or_replayed },
        }
        var adm: Admission = .{ .session_key = sessionKey(&self.secret, peer_id, &prove.head.server_nonce, &prove.head.peer_nonce, binding), .peer_id_len = @intCast(peer_id.len), .peer_id = undefined };
        @memcpy(adm.peer_id[0..peer_id.len], peer_id);
        return .{ .admitted = adm };
    }
};

/// The peer side: answer a challenge; the session key is derived here too.
pub const Answer = struct { prove_head: verbs.ProveHead, session_key: SessionKey };

pub fn answer(io: Io, secret: *const Secret, peer_id: []const u8, ch: verbs.Challenge, binding: []const u8) (Io.RandomSecureError || error{SchemaMismatch})!Answer {
    if (ch.schema != verbs.CLUSTER_WIRE_SCHEMA) return error.SchemaMismatch;
    std.debug.assert(peer_id.len >= 1 and peer_id.len <= verbs.PEER_ID_MAX);
    var peer_nonce: Nonce = undefined;
    try io.randomSecure(&peer_nonce);
    return .{
        .prove_head = .{ .schema = verbs.CLUSTER_WIRE_SCHEMA, .peer_id_len = @intCast(peer_id.len), .reserved = 0, .server_nonce = ch.server_nonce, .peer_nonce = peer_nonce, .mac = proof(secret, peer_id, &ch.server_nonce, &peer_nonce) },
        .session_key = sessionKey(secret, peer_id, &ch.server_nonce, &peer_nonce, binding),
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "the job:enroll record round-trips; a tampered byte, a wrong magic/version, a reserved bit and a wrong length are Corrupt — never re-minted" {
    const secret: Secret = @splat(0x5A);
    const bytes = encodeEnroll(secret);
    try testing.expectEqual(secret, try decodeEnroll(&bytes));
    var bad = bytes;
    bad[20] ^= 1;
    try testing.expectError(error.Corrupt, decodeEnroll(&bad));
    bad = bytes;
    bad[0] = 'X';
    try testing.expectError(error.Corrupt, decodeEnroll(&bad));
    bad = bytes;
    bad[4] = 2;
    try testing.expectError(error.Corrupt, decodeEnroll(&bad));
    bad = bytes;
    bad[6] = 1;
    try testing.expectError(error.Corrupt, decodeEnroll(&bad));
    try testing.expectError(error.Corrupt, decodeEnroll(bytes[0..47]));
    const minted = try mintSecret(testing.io);
    const minted2 = try mintSecret(testing.io);
    try testing.expect(!std.mem.eql(u8, &minted, &minted2));
}

test "the proof requires the secret; the session key binds the secret, both nonces, the peer id and the channel binding" {
    const s1: Secret = @splat(1);
    const s2: Secret = @splat(2);
    const n1: Nonce = @splat(3);
    const n2: Nonce = @splat(4);
    const p1 = proof(&s1, "peer-a", &n1, &n2);
    try testing.expect(!std.mem.eql(u8, &p1, &proof(&s2, "peer-a", &n1, &n2)));
    try testing.expect(!std.mem.eql(u8, &p1, &proof(&s1, "peer-b", &n1, &n2)));
    try testing.expect(!std.mem.eql(u8, &p1, &proof(&s1, "peer-a", &n2, &n1)));
    try testing.expectEqual(p1, proof(&s1, "peer-a", &n1, &n2));
    const k = sessionKey(&s1, "peer-a", &n1, &n2, "");
    try testing.expectEqual(k, sessionKey(&s1, "peer-a", &n1, &n2, ""));
    try testing.expect(!std.mem.eql(u8, &k, &sessionKey(&s2, "peer-a", &n1, &n2, "")));
    try testing.expect(!std.mem.eql(u8, &k, &sessionKey(&s1, "peer-b", &n1, &n2, "")));
    try testing.expect(!std.mem.eql(u8, &k, &sessionKey(&s1, "peer-a", &n2, &n1, "")));
    const binding: [BINDING_BYTES]u8 = @splat(9);
    try testing.expect(!std.mem.eql(u8, &k, &sessionKey(&s1, "peer-a", &n1, &n2, &binding)));
}

test "the nonce registry: single use, freshness-windowed, bounded by the cap with oldest-first eviction" {
    var reg = try NonceRegistry.init(testing.allocator, 4);
    defer reg.deinit(testing.allocator);
    const a: Nonce = @splat(1);
    reg.issue(a, 1000);
    try testing.expectEqual(@as(u32, 1), reg.liveCount());
    try testing.expectEqual(NonceVerdict.fresh, reg.consume(&a, 1500));
    try testing.expectEqual(NonceVerdict.unknown_or_replayed, reg.consume(&a, 1500));
    try testing.expectEqual(@as(u32, 0), reg.liveCount());
    const b: Nonce = @splat(2);
    reg.issue(b, 1000);
    try testing.expectEqual(NonceVerdict.expired, reg.consume(&b, 1000 + ENROLL_FRESHNESS_MS + 1));
    // Cap: 4 live, the 5th evicts the oldest (nonce 10).
    var i: u8 = 10;
    while (i < 15) : (i += 1) reg.issue(@splat(i), 2000 + @as(u64, i));
    try testing.expectEqual(@as(u32, 4), reg.liveCount());
    const evicted: Nonce = @splat(10);
    try testing.expectEqual(NonceVerdict.unknown_or_replayed, reg.consume(&evicted, 2100));
    const kept: Nonce = @splat(11);
    try testing.expectEqual(NonceVerdict.fresh, reg.consume(&kept, 2100));
    try testing.expectEqual(@as(u32, 64), nonceCap(1));
    try testing.expectEqual(@as(u32, 2048), nonceCap(512));
}

test "the ladder: an enrolled peer is admitted and both sides hold one key; a tampered proof is refused BEFORE the nonce is spent; a replayed proof is refused; a wrong secret and a foreign schema are refused" {
    const secret: Secret = @splat(0x33);
    var gate = try Gate.init(testing.allocator, secret, 8);
    defer gate.deinit(testing.allocator);
    const id: verbs.WireIdentity = .{ .node_token = 7, .writer_id = 1, .mount_slot = 2, .reserved = 0 };
    const peer_id = id.peerId();
    const ch = try gate.challenge(testing.io, 1000);
    try testing.expectEqual(@as(u32, 1), gate.nonces.liveCount());
    const ans = try answer(testing.io, &secret, &peer_id, ch, "");
    // Tamper one byte of the proof: refused, and the nonce is STILL live.
    var tampered = ans.prove_head;
    tampered.mac[0] ^= 1;
    try testing.expectEqual(RefuseReason.mac, gate.verify(.{ .head = tampered, .bytes = &peer_id }, "", 1001).refused);
    try testing.expectEqual(@as(u32, 1), gate.nonces.liveCount());
    // A different peer id under the right mac: the mac covers the id.
    try testing.expectEqual(RefuseReason.mac, gate.verify(.{ .head = ans.prove_head, .bytes = "someone-else" }, "", 1001).refused);
    // The honest proof: admitted, the nonce spent, the keys agree.
    const v = gate.verify(.{ .head = ans.prove_head, .bytes = &peer_id }, "", 1001);
    try testing.expectEqual(ans.session_key, v.admitted.session_key);
    try testing.expectEqualSlices(u8, &peer_id, v.admitted.peerId());
    try testing.expect(v.admitted.member().?.sameMember(id));
    try testing.expectEqual(@as(u32, 0), gate.nonces.liveCount());
    // Replay: the same proof again.
    try testing.expectEqual(RefuseReason.nonce_unknown_or_replayed, gate.verify(.{ .head = ans.prove_head, .bytes = &peer_id }, "", 1002).refused);
    // A wrong secret.
    const wrong: Secret = @splat(0x44);
    const ch2 = try gate.challenge(testing.io, 2000);
    const bad = try answer(testing.io, &wrong, &peer_id, ch2, "");
    try testing.expectEqual(RefuseReason.mac, gate.verify(.{ .head = bad.prove_head, .bytes = &peer_id }, "", 2001).refused);
    // An expired challenge: the mac is right, the window is not.
    const good2 = try answer(testing.io, &secret, &peer_id, ch2, "");
    try testing.expectEqual(RefuseReason.nonce_expired, gate.verify(.{ .head = good2.prove_head, .bytes = &peer_id }, "", 2000 + ENROLL_FRESHNESS_MS + 1).refused);
    // A foreign schema on either side.
    var foreign = ans.prove_head;
    foreign.schema = 7;
    try testing.expectEqual(RefuseReason.schema, gate.verify(.{ .head = foreign, .bytes = &peer_id }, "", 1).refused);
    var fch = ch;
    fch.schema = 5;
    try testing.expectError(error.SchemaMismatch, answer(testing.io, &secret, &peer_id, fch, ""));
    // An empty peer id.
    try testing.expectEqual(RefuseReason.peer_id_bounds, gate.verify(.{ .head = ans.prove_head, .bytes = "" }, "", 1).refused);
}

fn enrollFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [128]u8 = undefined;
    const len = smith.slice(&buf);
    _ = decodeEnroll(buf[0..len]) catch {};
}

test "fuzz: the enroll decoder is total" {
    try fuzzkit.fuzz({}, enrollFuzz, .{});
}
