//! The cluster's own PKI (charter §8 "remote workers over TCP/TLS";
//! decision 0003 §N): ONE profile — Ed25519 everywhere — issued by this
//! binary (`squeezefs key tls-init` mints the cluster CA, `key tls-issue`
//! a node certificate under it) and consumed by `wire/tls.zig`'s
//! `MutualTls` channel, which pins the CA and accepts exactly a one-leaf
//! chain under it.
//!
//! Why Ed25519 and nothing else: 32 B keys, 64 B signatures, deterministic
//! signing (no nonce to mis-generate), the fastest verify in `std.crypto`,
//! and `std.crypto.Certificate` already parses and verifies Ed25519
//! certificates — so the wire needs no second signature stack and the
//! peer's CertificateVerify and its certificate ride ONE primitive.
//!
//! The encoder writes the minimal X.509 v3 DER `std.crypto.Certificate.parse`
//! and OpenSSL both accept: version, serial, the Ed25519 signature
//! algorithm, `CN=<name>` issuer and subject, a UTCTime/GeneralizedTime
//! validity, the Ed25519 SubjectPublicKeyInfo, and two extensions
//! (`basicConstraints` CA:TRUE + `keyUsage` keyCertSign|cRLSign on the CA;
//! `keyUsage` digitalSignature on a node). The files are PEM so the
//! operator's tools read them; the private key is the 48 B PKCS#8 v1
//! Ed25519 form `openssl genpkey -algorithm ed25519` emits. Every decoder
//! is total and fuzzed.
//!
//! Portable; randomness through the `Io` the caller passes.

const std = @import("std");
const Io = std.Io;
const Certificate = std.crypto.Certificate;
const fuzzkit = @import("../core/fuzzkit.zig");

pub const Ed25519 = std.crypto.sign.Ed25519;
pub const KeyPair = Ed25519.KeyPair;
pub const SEED_BYTES: usize = Ed25519.KeyPair.seed_length;
pub const PUBLIC_BYTES: usize = Ed25519.PublicKey.encoded_length;
pub const SIGNATURE_BYTES: usize = Ed25519.Signature.encoded_length;

/// A certificate this encoder emits is under 512 B (two short names, two
/// extensions); the bound is generous for what the LOADER accepts from a
/// file so a foreign PKI's leaf (an RSA-4096 chain member is ~1.5 KiB) is
/// refused by the profile, never by a buffer.
pub const CERT_MAX_DER: usize = 4096;
/// `CN=` names: a hostname-class label (RFC 1035's 63-octet label bound).
pub const NAME_MAX: usize = 63;
pub const SERIAL_BYTES: usize = 16;

/// RFC 5280 §4.1.2.5: dates through 2049 are UTCTime, later ones
/// GeneralizedTime (the encoder follows it; the parser reads both).
const UTC_TIME_LAST_YEAR: u16 = 2049;

comptime {
    std.debug.assert(SEED_BYTES == 32 and PUBLIC_BYTES == 32 and SIGNATURE_BYTES == 64);
}

// ---------------------------------------------------------------------------
// DER, written backwards: a TLV's length is known when it closes, so the
// encoder fills a fixed buffer from its END and prefixes each header once.
// ---------------------------------------------------------------------------

/// Universal and context tags (X.690 §8; the two context tags are the
/// certificate's `[0] version` and `[3] extensions`).
const Tag = enum(u8) {
    boolean = 0x01,
    integer = 0x02,
    bit_string = 0x03,
    octet_string = 0x04,
    oid = 0x06,
    utf8_string = 0x0C,
    sequence = 0x30,
    set = 0x31,
    utc_time = 0x17,
    generalized_time = 0x18,
    ctx0_version = 0xA0,
    ctx3_extensions = 0xA3,
};

/// Object identifiers in their DER contents form.
const oid_ed25519 = [_]u8{ 0x2B, 0x65, 0x70 }; // 1.3.101.112 (RFC 8410)
const oid_common_name = [_]u8{ 0x55, 0x04, 0x03 }; // 2.5.4.3
const oid_basic_constraints = [_]u8{ 0x55, 0x1D, 0x13 }; // 2.5.29.19
const oid_key_usage = [_]u8{ 0x55, 0x1D, 0x0F }; // 2.5.29.15

const DerError = error{Overflow};

const Der = struct {
    buf: []u8,
    /// Everything at `buf[pos..]` is written; the next byte goes at `pos - 1`.
    pos: usize,

    fn init(buf: []u8) Der {
        return .{ .buf = buf, .pos = buf.len };
    }

    fn written(self: *const Der) []const u8 {
        return self.buf[self.pos..];
    }

    fn prepend(self: *Der, bytes: []const u8) DerError!void {
        if (bytes.len > self.pos) return error.Overflow;
        self.pos -= bytes.len;
        @memcpy(self.buf[self.pos..][0..bytes.len], bytes);
    }

    fn prependByte(self: *Der, b: u8) DerError!void {
        try self.prepend(&[_]u8{b});
    }

    /// Close a TLV whose contents start at `self.pos` and end at `end`.
    fn header(self: *Der, tag: Tag, end: usize) DerError!void {
        const len = end - self.pos;
        // X.690 §8.1.3: short form below 128, else `0x80 | n` then n bytes.
        if (len < 0x80) {
            try self.prependByte(@intCast(len));
        } else if (len <= std.math.maxInt(u8)) {
            try self.prependByte(@intCast(len));
            try self.prependByte(0x81);
        } else if (len <= std.math.maxInt(u16)) {
            var w: [2]u8 = undefined;
            std.mem.writeInt(u16, &w, @intCast(len), .big);
            try self.prepend(&w);
            try self.prependByte(0x82);
        } else return error.Overflow;
        try self.prependByte(@backingInt(tag));
    }

    fn primitive(self: *Der, tag: Tag, contents: []const u8) DerError!void {
        const end = self.pos;
        try self.prepend(contents);
        try self.header(tag, end);
    }

    fn boolean(self: *Der, v: bool) DerError!void {
        try self.primitive(.boolean, &[_]u8{if (v) 0xFF else 0x00});
    }

    /// A positive INTEGER: a leading zero when the top bit is set (X.690 §8.3).
    fn unsignedInteger(self: *Der, magnitude: []const u8) DerError!void {
        const end = self.pos;
        var i: usize = 0;
        while (i + 1 < magnitude.len and magnitude[i] == 0) i += 1;
        const m = magnitude[i..];
        try self.prepend(m);
        if (m[0] & 0x80 != 0) try self.prependByte(0);
        try self.header(.integer, end);
    }

    fn bitString(self: *Der, unused_bits: u8, bits: []const u8) DerError!void {
        const end = self.pos;
        try self.prepend(bits);
        try self.prependByte(unused_bits);
        try self.header(.bit_string, end);
    }

    /// `SEQUENCE { OID ed25519 }` — RFC 8410 §3: no parameters.
    fn algorithmEd25519(self: *Der) DerError!void {
        const end = self.pos;
        try self.primitive(.oid, &oid_ed25519);
        try self.header(.sequence, end);
    }

    /// `Name ::= SEQUENCE { SET { SEQUENCE { OID cn, UTF8String name } } }`.
    fn commonName(self: *Der, name: []const u8) DerError!void {
        const end = self.pos;
        try self.primitive(.utf8_string, name);
        try self.primitive(.oid, &oid_common_name);
        try self.header(.sequence, end);
        try self.header(.set, end);
        try self.header(.sequence, end);
    }

    fn time(self: *Der, unix_s: u64) DerError!void {
        const d = civilFromUnix(unix_s);
        var text: [15]u8 = undefined;
        if (d.year <= UTC_TIME_LAST_YEAR) {
            const t = std.fmt.bufPrint(&text, "{d:0>2}{d:0>2}{d:0>2}{d:0>2}{d:0>2}{d:0>2}Z", .{ d.year % 100, d.month, d.day, d.hour, d.minute, d.second }) catch unreachable;
            try self.primitive(.utc_time, t);
        } else {
            const t = std.fmt.bufPrint(&text, "{d:0>4}{d:0>2}{d:0>2}{d:0>2}{d:0>2}{d:0>2}Z", .{ d.year, d.month, d.day, d.hour, d.minute, d.second }) catch unreachable;
            try self.primitive(.generalized_time, t);
        }
    }

    /// `Extension ::= SEQUENCE { OID, BOOLEAN critical, OCTET STRING value }`.
    fn extension(self: *Der, oid: []const u8, critical: bool, value: []const u8) DerError!void {
        const end = self.pos;
        try self.primitive(.octet_string, value);
        if (critical) try self.boolean(true);
        try self.primitive(.oid, oid);
        try self.header(.sequence, end);
    }
};

const Civil = struct { year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8 };

/// Proleptic Gregorian civil date from Unix seconds (Howard Hinnant's
/// `civil_from_days`; exact for every date the format can name).
fn civilFromUnix(unix_s: u64) Civil {
    const s_per_day = std.time.s_per_day;
    const days: i64 = @intCast(unix_s / s_per_day);
    const rem: u64 = unix_s % s_per_day;
    const z = days + 719_468; // shift the epoch to 0000-03-01
    const era = @divFloor(z, 146_097); // days per 400-year era
    const doe: u64 = @intCast(z - era * 146_097);
    const yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    const doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    const mp = (5 * doy + 2) / 153;
    const day: u8 = @intCast(doy - (153 * mp + 2) / 5 + 1);
    const month: u8 = @intCast(if (mp < 10) mp + 3 else mp - 9);
    const year: i64 = @as(i64, @intCast(yoe)) + era * 400 + @intFromBool(month <= 2);
    return .{
        .year = @intCast(year),
        .month = month,
        .day = day,
        .hour = @intCast(rem / std.time.s_per_hour),
        .minute = @intCast((rem % std.time.s_per_hour) / std.time.s_per_min),
        .second = @intCast(rem % std.time.s_per_min),
    };
}

// ---------------------------------------------------------------------------
// Issuing.
// ---------------------------------------------------------------------------

pub const IssueError = error{ NameInvalid, ValidityInvalid, Overflow, SigningFailed };

pub const Role = enum { ca, node };

pub const Request = struct {
    role: Role,
    /// The subject's `CN`.
    name: []const u8,
    /// The issuer's `CN` (the CA's own name on a CA certificate).
    issuer_name: []const u8,
    serial: [SERIAL_BYTES]u8,
    not_before_s: u64,
    not_after_s: u64,
    subject_public: [PUBLIC_BYTES]u8,
};

/// A `CN` is 1..NAME_MAX printable ASCII bytes (no control characters:
/// the name is rendered into logs verbatim).
pub fn nameValid(name: []const u8) bool {
    if (name.len == 0 or name.len > NAME_MAX) return false;
    for (name) |c| if (c < 0x20 or c > 0x7E) return false;
    return true;
}

/// Encode and sign one certificate with `issuer_key` into `out`; returns
/// the DER. `out.len ≥ CERT_MAX_DER`.
pub fn issue(req: Request, issuer_key: *const KeyPair, out: []u8) IssueError![]const u8 {
    if (!nameValid(req.name) or !nameValid(req.issuer_name)) return error.NameInvalid;
    if (req.not_after_s <= req.not_before_s) return error.ValidityInvalid;
    // The TBSCertificate first, into the END of `out`, so it can be signed
    // in place and then wrapped.
    var tbs = Der.init(out);
    const tbs_end = tbs.pos;
    {
        // extensions [3]
        const ext_end = tbs.pos;
        switch (req.role) {
            .ca => {
                // keyUsage keyCertSign(5) | cRLSign(6): bits 5–6 of the
                // first octet = 0x06 with 1 unused bit.
                try tbs.extension(&oid_key_usage, true, &[_]u8{ @backingInt(Tag.bit_string), 2, 1, 0x06 });
                // basicConstraints { cA TRUE }.
                try tbs.extension(&oid_basic_constraints, true, &[_]u8{ @backingInt(Tag.sequence), 3, @backingInt(Tag.boolean), 1, 0xFF });
            },
            // keyUsage digitalSignature(0): bit 0 = 0x80 with 7 unused bits.
            .node => try tbs.extension(&oid_key_usage, true, &[_]u8{ @backingInt(Tag.bit_string), 2, 7, 0x80 }),
        }
        try tbs.header(.sequence, ext_end);
        try tbs.header(.ctx3_extensions, ext_end);
        // subjectPublicKeyInfo
        const spki_end = tbs.pos;
        try tbs.bitString(0, &req.subject_public);
        try tbs.algorithmEd25519();
        try tbs.header(.sequence, spki_end);
        try tbs.commonName(req.name);
        // validity
        const val_end = tbs.pos;
        try tbs.time(req.not_after_s);
        try tbs.time(req.not_before_s);
        try tbs.header(.sequence, val_end);
        try tbs.commonName(req.issuer_name);
        try tbs.algorithmEd25519();
        try tbs.unsignedInteger(&req.serial);
        // version [0] { INTEGER 2 } = v3
        const ver_end = tbs.pos;
        try tbs.primitive(.integer, &[_]u8{2});
        try tbs.header(.ctx0_version, ver_end);
        try tbs.header(.sequence, tbs_end);
    }
    const tbs_bytes = tbs.written();
    const sig = issuer_key.sign(tbs_bytes, null) catch return error.SigningFailed;
    const sig_bytes = sig.toBytes();
    // Certificate ::= SEQUENCE { tbs, algorithm, BIT STRING signature }:
    // the trailer (7 B algorithm + 67 B signature) is built aside, the
    // TBS slides down to make room for it at the end, the outer header
    // goes on, and the whole moves to the buffer's start.
    var trailer_buf: [80]u8 = undefined;
    var tr = Der.init(&trailer_buf);
    try tr.bitString(0, &sig_bytes);
    try tr.algorithmEd25519();
    const trailer = tr.written();
    const tbs_len = tbs_bytes.len;
    if (tbs.pos < trailer.len + 4) return error.Overflow; // room for the outer header (≤ 4 B)
    const contents_start = tbs.pos - trailer.len;
    std.mem.copyForwards(u8, out[contents_start..][0..tbs_len], tbs_bytes);
    @memcpy(out[contents_start + tbs_len ..][0..trailer.len], trailer);
    var outer = Der.init(out);
    outer.pos = contents_start;
    try outer.header(.sequence, out.len);
    const der = outer.written();
    std.mem.copyForwards(u8, out[0..der.len], der);
    return out[0..der.len];
}

/// A fresh serial: 16 random bytes, the top bit cleared and the next set
/// (positive, no leading zero, never zero).
pub fn mintSerial(io: Io) Io.RandomSecureError![SERIAL_BYTES]u8 {
    var s: [SERIAL_BYTES]u8 = undefined;
    try io.randomSecure(&s);
    s[0] = (s[0] & 0x7F) | 0x40;
    return s;
}

pub fn mintKeyPair(io: Io) KeyPair {
    return KeyPair.generate(io);
}

// ---------------------------------------------------------------------------
// Parsing and verification (over `std.crypto.Certificate`).
// ---------------------------------------------------------------------------

pub const ParseError = error{ CertificateMalformed, CertificateNotEd25519, CertificateTooLarge };

/// The parsed view of a DER certificate; the `der` slice must outlive it.
pub const Parsed = struct {
    der: []const u8,
    parsed: Certificate.Parsed,

    pub fn commonName(self: *const Parsed) []const u8 {
        return self.parsed.commonName();
    }

    pub fn publicKey(self: *const Parsed) *const [PUBLIC_BYTES]u8 {
        return self.parsed.pubKey()[0..PUBLIC_BYTES];
    }

    pub fn notAfterS(self: *const Parsed) u64 {
        return self.parsed.validity.not_after;
    }

    pub fn notBeforeS(self: *const Parsed) u64 {
        return self.parsed.validity.not_before;
    }
};

/// Total: a DER certificate under the profile (Ed25519 key AND Ed25519
/// signature) parses; anything else is refused by name. `std.crypto.
/// Certificate.parse` indexes its buffer unchecked (a short or lying
/// length is a panic, not an error), so `Screen` walks the exact schema
/// it will walk, bounds-checked, BEFORE it runs.
pub fn parseCertificate(der: []const u8) ParseError!Parsed {
    if (der.len > CERT_MAX_DER) return error.CertificateTooLarge;
    try Screen.run(der);
    const cert: Certificate = .{ .buffer = der, .index = 0 };
    const p = cert.parse() catch return error.CertificateMalformed;
    if (p.pub_key_algo != .curveEd25519 or p.signature_algorithm != .curveEd25519) return error.CertificateNotEd25519;
    if (p.pubKey().len != PUBLIC_BYTES) return error.CertificateMalformed;
    return .{ .der = der, .parsed = p };
}

/// The total pre-walk of `std.crypto.Certificate.parse`'s schema: every
/// element it will read is in bounds and inside its parent, every BIT
/// STRING it indexes is non-empty, and the two algorithm identifiers are
/// Ed25519 (so its EC parameter branch never runs). Mirrors the parser's
/// own element grammar (short form, long form of ≤ 4 length bytes).
const Screen = struct {
    const E = struct { tag: u8, start: usize, end: usize };
    const Err = error{CertificateMalformed};

    bytes: []const u8,

    fn elem(self: Screen, index: usize, parent_end: usize) Err!E {
        const b = self.bytes;
        if (index + 2 > parent_end) return error.CertificateMalformed;
        const tag = b[index];
        const size_byte = b[index + 1];
        var i = index + 2;
        var end: usize = undefined;
        if (size_byte >> 7 == 0) {
            end = i + size_byte;
        } else {
            const len_size: usize = size_byte & 0x7F;
            if (len_size == 0 or len_size > @sizeOf(u32)) return error.CertificateMalformed;
            if (i + len_size > parent_end) return error.CertificateMalformed;
            var long: usize = 0;
            for (b[i .. i + len_size]) |x| long = (long << 8) | x;
            i += len_size;
            end = i + long;
        }
        if (end > parent_end) return error.CertificateMalformed;
        return .{ .tag = tag, .start = i, .end = end };
    }

    fn contents(self: Screen, e: E) []const u8 {
        return self.bytes[e.start..e.end];
    }

    /// A BIT STRING the parser reads `buffer[start]` of: at least the
    /// unused-bits octet must exist.
    fn bitString(e: E) Err!void {
        if (e.tag != @backingInt(Tag.bit_string) or e.end == e.start) return error.CertificateMalformed;
    }

    /// `AlgorithmIdentifier ::= SEQUENCE { OID … }` whose OID is Ed25519.
    fn ed25519Algorithm(self: Screen, alg: E) Err!void {
        const oid = try self.elem(alg.start, alg.end);
        if (oid.tag != @backingInt(Tag.oid) or !std.mem.eql(u8, self.contents(oid), &oid_ed25519)) return error.CertificateMalformed;
    }

    /// The name walk the parser does on the subject (three nested loops,
    /// each child read at its sibling's end).
    fn name(self: Screen, n: E) Err!void {
        var name_i = n.start;
        while (name_i < n.end) {
            const rdn = try self.elem(name_i, n.end);
            var rdn_i = rdn.start;
            while (rdn_i < rdn.end) {
                const atav = try self.elem(rdn_i, rdn.end);
                var atav_i = atav.start;
                while (atav_i < atav.end) {
                    const ty = try self.elem(atav_i, atav.end);
                    const val = try self.elem(ty.end, atav.end);
                    atav_i = val.end;
                }
                rdn_i = atav.end;
            }
            name_i = rdn.end;
        }
    }

    fn run(bytes: []const u8) Err!void {
        const s: Screen = .{ .bytes = bytes };
        const cert = try s.elem(0, bytes.len);
        // The outer SEQUENCE is the whole buffer (a certificate followed by
        // anything is not one certificate).
        if (cert.end != bytes.len) return error.CertificateMalformed;
        const tbs = try s.elem(cert.start, cert.end);
        const version = try s.elem(tbs.start, tbs.end);
        const serial = if (version.tag == @backingInt(Tag.ctx0_version)) try s.elem(version.end, tbs.end) else version;
        const tbs_sig = try s.elem(serial.end, tbs.end);
        try s.ed25519Algorithm(tbs_sig);
        const issuer = try s.elem(tbs_sig.end, tbs.end);
        const validity = try s.elem(issuer.end, tbs.end);
        const not_before = try s.elem(validity.start, validity.end);
        _ = try s.elem(not_before.end, validity.end);
        const subject = try s.elem(validity.end, tbs.end);
        try s.name(subject);
        const spki = try s.elem(subject.end, tbs.end);
        const spki_alg = try s.elem(spki.start, spki.end);
        try s.ed25519Algorithm(spki_alg);
        try bitString(try s.elem(spki_alg.end, spki.end));
        if (spki.end < tbs.end) {
            const outer = try s.elem(spki.end, tbs.end);
            // The parser enters the extensions on tag NUMBER 3 alone
            // ([3] EXPLICIT and BIT STRING share it).
            if (outer.tag & 0x1F == @backingInt(Tag.bit_string)) {
                const exts = try s.elem(outer.start, outer.end);
                var ext_i = exts.start;
                while (ext_i < exts.end) {
                    const ext = try s.elem(ext_i, exts.end);
                    ext_i = ext.end;
                    const oid = try s.elem(ext.start, ext.end);
                    const critical = try s.elem(oid.end, ext.end);
                    if (critical.tag == @backingInt(Tag.boolean)) _ = try s.elem(critical.end, ext.end);
                }
            }
        }
        const sig_alg = try s.elem(tbs.end, cert.end);
        try s.ed25519Algorithm(sig_alg);
        try bitString(try s.elem(sig_alg.end, cert.end));
    }
};

pub const VerifyError = error{ Untrusted, Expired, NotYetValid };

/// `leaf` is issued by `ca` (issuer name, Ed25519 signature) and valid at
/// `now_s`. The three outcomes are the three counted refusals.
pub fn verifyLeaf(leaf: *const Parsed, ca: *const Parsed, now_s: u64) VerifyError!void {
    leaf.parsed.verify(ca.parsed, @intCast(now_s)) catch |e| return switch (e) {
        error.CertificateExpired => error.Expired,
        error.CertificateNotYetValid => error.NotYetValid,
        else => error.Untrusted,
    };
}

// ---------------------------------------------------------------------------
// PEM and the PKCS#8 private key.
// ---------------------------------------------------------------------------

pub const PEM_CERTIFICATE = "CERTIFICATE";
pub const PEM_PRIVATE_KEY = "PRIVATE KEY";

/// RFC 7468 §2: 64 base64 characters per line.
const PEM_LINE = 64;

/// `-----BEGIN label-----\n…\n-----END label-----\n`; returns the text.
pub fn pemEncode(label: []const u8, der: []const u8, out: []u8) error{Overflow}![]const u8 {
    var w: Io.Writer = .fixed(out);
    w.print("-----BEGIN {s}-----\n", .{label}) catch return error.Overflow;
    const enc = std.base64.standard.Encoder;
    var line: [PEM_LINE]u8 = undefined;
    var i: usize = 0;
    // 48 raw bytes encode to one 64-character line.
    const raw_per_line = PEM_LINE / 4 * 3;
    while (i < der.len) : (i += raw_per_line) {
        const chunk = der[i..@min(i + raw_per_line, der.len)];
        const text = enc.encode(&line, chunk);
        w.print("{s}\n", .{text}) catch return error.Overflow;
    }
    w.print("-----END {s}-----\n", .{label}) catch return error.Overflow;
    return w.buffered();
}

pub const PemError = error{ PemMalformed, PemLabelMismatch, Overflow };

/// The FIRST block of `label` in `text`, base64-decoded into `out`
/// (whitespace inside the body ignored; a block of another label is a
/// mismatch, never silently skipped — a key file holding a certificate is
/// a misconfiguration to name).
pub fn pemDecode(label: []const u8, text: []const u8, out: []u8) PemError![]const u8 {
    const begin_mark = "-----BEGIN ";
    const end_mark = "-----END ";
    const b = std.mem.indexOf(u8, text, begin_mark) orelse return error.PemMalformed;
    const label_start = b + begin_mark.len;
    const label_end = std.mem.indexOfPos(u8, text, label_start, "-----") orelse return error.PemMalformed;
    const found = text[label_start..label_end];
    if (!std.mem.eql(u8, found, label)) return error.PemLabelMismatch;
    const body_start = label_end + "-----".len;
    const e = std.mem.indexOfPos(u8, text, body_start, end_mark) orelse return error.PemMalformed;
    const end_label_start = e + end_mark.len;
    const end_label_end = std.mem.indexOfPos(u8, text, end_label_start, "-----") orelse return error.PemMalformed;
    if (!std.mem.eql(u8, text[end_label_start..end_label_end], label)) return error.PemMalformed;
    const body = text[body_start..e];
    const dec = std.base64.standard.decoderWithIgnore(" \t\r\n");
    const n = dec.calcSizeUpperBound(body.len);
    if (n > out.len) return error.Overflow;
    const written = dec.decode(out, body) catch return error.PemMalformed;
    return out[0..written];
}

/// PKCS#8 v1 `PrivateKeyInfo` for Ed25519 (RFC 8410 §7): a fixed 16 B
/// prefix and the 32 B seed as `OCTET STRING { OCTET STRING seed }`.
pub const PKCS8_PREFIX = [_]u8{ 0x30, 0x2E, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2B, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20 };
pub const PKCS8_LEN: usize = PKCS8_PREFIX.len + SEED_BYTES;

pub fn encodePkcs8(seed: *const [SEED_BYTES]u8) [PKCS8_LEN]u8 {
    var out: [PKCS8_LEN]u8 = undefined;
    out[0..PKCS8_PREFIX.len].* = PKCS8_PREFIX;
    out[PKCS8_PREFIX.len..].* = seed.*;
    return out;
}

/// Total: exactly the v1 form decodes (the v2 form with a public key
/// attribute, any other algorithm, any other length are `KeyMalformed`).
pub fn decodePkcs8(der: []const u8) error{KeyMalformed}![SEED_BYTES]u8 {
    if (der.len != PKCS8_LEN) return error.KeyMalformed;
    if (!std.mem.eql(u8, der[0..PKCS8_PREFIX.len], &PKCS8_PREFIX)) return error.KeyMalformed;
    return der[PKCS8_PREFIX.len..][0..SEED_BYTES].*;
}

pub fn keyPairFromSeed(seed: *const [SEED_BYTES]u8) error{KeyMalformed}!KeyPair {
    return KeyPair.generateDeterministic(seed.*) catch error.KeyMalformed;
}

/// The seed of a key pair (what the PKCS#8 file stores).
pub fn seedOf(kp: *const KeyPair) [SEED_BYTES]u8 {
    return kp.secret_key.seed();
}

/// A PEM private key file's text → the key pair; the public key must be
/// the certificate's when the caller pairs them (`tls.Identity.init`).
pub fn parsePrivateKeyPem(text: []const u8) (PemError || error{KeyMalformed})!KeyPair {
    var der: [PKCS8_LEN + 1]u8 = undefined;
    const bytes = try pemDecode(PEM_PRIVATE_KEY, text, &der);
    const seed = try decodePkcs8(bytes);
    defer std.crypto.secureZero(u8, &der);
    return keyPairFromSeed(&seed);
}

/// A PEM certificate file's text → the DER (into `out`, ≥ CERT_MAX_DER).
pub fn parseCertificatePem(text: []const u8, out: []u8) (PemError || ParseError)!Parsed {
    std.debug.assert(out.len >= CERT_MAX_DER);
    const der = pemDecode(PEM_CERTIFICATE, text, out) catch |e| return switch (e) {
        error.Overflow => error.CertificateTooLarge,
        else => |x| x,
    };
    return parseCertificate(der);
}

// ---------------------------------------------------------------------------
// A whole PKI in memory (the tests', the smoke's generator, the verbs').
// ---------------------------------------------------------------------------

/// One year of validity for a node certificate, ten for the CA: the CA
/// outlives every node it signs; a node's cert is re-issued with the
/// operator's annual rotation (the key stays, the serial moves).
pub const NODE_VALIDITY_S: u64 = 365 * std.time.s_per_day;
pub const CA_VALIDITY_S: u64 = 10 * 365 * std.time.s_per_day;

pub const Issued = struct {
    der_buf: [CERT_MAX_DER]u8,
    der_len: usize,
    key: KeyPair,

    pub fn der(self: *const Issued) []const u8 {
        return self.der_buf[0..self.der_len];
    }
};

/// Mint a CA named `name`, valid from `now_s`.
pub fn mintCa(io: Io, name: []const u8, now_s: u64) (IssueError || Io.RandomSecureError)!Issued {
    var out: Issued = .{ .der_buf = undefined, .der_len = 0, .key = mintKeyPair(io) };
    const der = try issue(.{ .role = .ca, .name = name, .issuer_name = name, .serial = try mintSerial(io), .not_before_s = now_s, .not_after_s = now_s + CA_VALIDITY_S, .subject_public = out.key.public_key.toBytes() }, &out.key, &out.der_buf);
    out.der_len = der.len;
    return out;
}

/// Mint a node certificate under `ca`, valid `[now_s, now_s + validity_s)`.
pub fn mintNode(io: Io, ca: *const Issued, ca_name: []const u8, name: []const u8, now_s: u64, validity_s: u64) (IssueError || Io.RandomSecureError)!Issued {
    var out: Issued = .{ .der_buf = undefined, .der_len = 0, .key = mintKeyPair(io) };
    const der = try issue(.{ .role = .node, .name = name, .issuer_name = ca_name, .serial = try mintSerial(io), .not_before_s = now_s, .not_after_s = now_s + validity_s, .subject_public = out.key.public_key.toBytes() }, &ca.key, &out.der_buf);
    out.der_len = der.len;
    return out;
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "civil dates: the epoch, a leap day, the UTCTime/GeneralizedTime boundary" {
    try testing.expectEqual(Civil{ .year = 1970, .month = 1, .day = 1, .hour = 0, .minute = 0, .second = 0 }, civilFromUnix(0));
    // 2024-02-29T12:34:56Z
    try testing.expectEqual(Civil{ .year = 2024, .month = 2, .day = 29, .hour = 12, .minute = 34, .second = 56 }, civilFromUnix(1_709_210_096));
    // 2050-01-01T00:00:00Z
    try testing.expectEqual(Civil{ .year = 2050, .month = 1, .day = 1, .hour = 0, .minute = 0, .second = 0 }, civilFromUnix(2_524_608_000));
}

test "a CA and a node certificate issue, parse under std.crypto.Certificate and verify; the CA is self-signed; a foreign CA, an expired leaf and a not-yet-valid leaf are the three refusals; a tampered byte is Untrusted" {
    const io = testing.io;
    const now: u64 = 1_760_000_000; // 2025-10
    const ca = try mintCa(io, "sqz-test-ca", now);
    const ca_p = try parseCertificate(ca.der());
    try testing.expectEqualStrings("sqz-test-ca", ca_p.commonName());
    try testing.expectEqual(ca.key.public_key.toBytes(), ca_p.publicKey().*);
    try verifyLeaf(&ca_p, &ca_p, now + 1);
    const node = try mintNode(io, &ca, "sqz-test-ca", "node-a", now, NODE_VALIDITY_S);
    const node_p = try parseCertificate(node.der());
    try testing.expectEqualStrings("node-a", node_p.commonName());
    try verifyLeaf(&node_p, &ca_p, now + 100);
    try testing.expectError(error.Expired, verifyLeaf(&node_p, &ca_p, now + NODE_VALIDITY_S + 1));
    try testing.expectError(error.NotYetValid, verifyLeaf(&node_p, &ca_p, now - 1));
    const other = try mintCa(io, "sqz-test-ca", now);
    const other_p = try parseCertificate(other.der());
    // Same issuer NAME, another key: the signature fails.
    try testing.expectError(error.Untrusted, verifyLeaf(&node_p, &other_p, now + 100));
    var tampered = node;
    tampered.der_buf[tampered.der_len / 2] ^= 1;
    if (parseCertificate(tampered.der())) |tp| {
        try testing.expectError(error.Untrusted, verifyLeaf(&tp, &ca_p, now + 100));
    } else |_| {}
    // A GeneralizedTime validity (the CA's ten years cross 2049 from 2040).
    const late = try mintCa(io, "late-ca", 2_220_000_000);
    const late_p = try parseCertificate(late.der());
    try testing.expectEqual(@as(u64, 2_220_000_000 + CA_VALIDITY_S), late_p.notAfterS());
    // Names: empty, too long, a control character.
    try testing.expectError(error.NameInvalid, mintCa(io, "", now));
    try testing.expectError(error.NameInvalid, mintCa(io, "a\nb", now));
    const long: [NAME_MAX + 1]u8 = @splat('x');
    try testing.expectError(error.NameInvalid, mintCa(io, &long, now));
    try testing.expectError(error.ValidityInvalid, mintNode(io, &ca, "sqz-test-ca", "n", now, 0));
}

test "PEM round-trips a certificate and a PKCS#8 key; the label is checked; whitespace is ignored; the v2 key form and a short key are refused" {
    const io = testing.io;
    const ca = try mintCa(io, "pem-ca", 1_760_000_000);
    var text: [4096]u8 = undefined;
    const pem = try pemEncode(PEM_CERTIFICATE, ca.der(), &text);
    try testing.expect(std.mem.startsWith(u8, pem, "-----BEGIN CERTIFICATE-----\n"));
    var der_buf: [CERT_MAX_DER]u8 = undefined;
    const back = try parseCertificatePem(pem, &der_buf);
    try testing.expectEqualSlices(u8, ca.der(), back.der);
    var key_text: [512]u8 = undefined;
    const seed = seedOf(&ca.key);
    const kpem = try pemEncode(PEM_PRIVATE_KEY, &encodePkcs8(&seed), &key_text);
    const kp = try parsePrivateKeyPem(kpem);
    try testing.expectEqual(ca.key.public_key.toBytes(), kp.public_key.toBytes());
    // A certificate handed to the key loader is a label mismatch.
    try testing.expectError(error.PemLabelMismatch, parsePrivateKeyPem(pem));
    // v2 form (the prefix differs) and a truncated seed.
    var v2 = encodePkcs8(&seed);
    v2[2] = 0x01;
    try testing.expectError(error.KeyMalformed, decodePkcs8(&v2));
    try testing.expectError(error.KeyMalformed, decodePkcs8(v2[0 .. PKCS8_LEN - 1]));
    // Trailing bytes after the certificate are not one certificate.
    var with_tail: [CERT_MAX_DER]u8 = undefined;
    @memcpy(with_tail[0..ca.der_len], ca.der());
    with_tail[ca.der_len] = 0;
    try testing.expectError(error.CertificateMalformed, parseCertificate(with_tail[0 .. ca.der_len + 1]));
}

fn pkiFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [1024]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseCertificate(buf[0..len]) catch {};
    _ = decodePkcs8(buf[0..len]) catch {};
    var der: [CERT_MAX_DER]u8 = undefined;
    _ = parseCertificatePem(buf[0..len], &der) catch {};
    _ = parsePrivateKeyPem(buf[0..len]) catch {};
    var out: [128]u8 = undefined;
    _ = pemDecode("X", buf[0..len], &out) catch {};
    _ = nameValid(buf[0..len]);
    if (len >= SEED_BYTES) _ = keyPairFromSeed(buf[0..SEED_BYTES]) catch {};
}

test "fuzz: the PKI decoders (certificate, PKCS#8, PEM) are total" {
    try fuzzkit.fuzz({}, pkiFuzz, .{});
}
