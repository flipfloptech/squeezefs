//! Key handling beyond the wrap (charter §6.5; `docs/porting/
//! ops_nvmet_keys.md` §7): the key store `/etc/squeezefs/keys/<key_id>.key`
//! managed by `key install | show | check`, `mount --encrypt-key <path|->`
//! with the PRE-FORK stash (the material is read in the INVOKING process
//! before `--daemon`'s first fork so `-` works for daemonized mounts and a
//! bad key file fails on the caller's console, not over the ready pipe;
//! the material crosses the forks IN MEMORY — never argv, never the pipe
//! — and every parent zeroizes its copy right after the fork returns),
//! and the zeroize audit's one addition: every buffer that holds material
//! is `mlock`ed and `MADV_DONTDUMP`ed (swap can spill what
//! `PR_SET_DUMPABLE` protects from a core; `MADV_DONTFORK` is NOT applied —
//! the daemon fork must inherit the material). No rotation (§7.4: a
//! defrag-class mover rewriting every block header; owed later, never a
//! cheaper rotation that leaves old headers readable under the retired
//! key).
//!
//! Portable here: the `SecretBuf` contract and the key-store path law;
//! the verbs (an offline set read + the FD laws + an atomic 0600 write
//! through the io_uring file layer) are Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;
const is_linux = builtin.os.tag == .linux;

const log = @import("../core/log.zig");
const transform = @import("../data/transform.zig");

/// The key store and its one law: dir 0700, files 0600, root-owned
/// (`transform.linux_keys.readKeyFile` checks owner ∈ {euid, 0} and
/// `mode & 0o077 == 0` on the FD at every read).
pub const KEY_DIR = transform.KEY_DIR;
pub const KEY_DIR_PARENT = "/etc/squeezefs";

/// A material buffer: zeroized on `deinit` whatever the path; on Linux
/// also pinned (`mlock`) and excluded from core dumps (`MADV_DONTDUMP`).
/// `≤ 64 KiB + 1` per buffer, inside any default `RLIMIT_MEMLOCK`.
pub const SecretBuf = struct {
    gpa: Allocator,
    bytes: []align(4096) u8,
    locked: bool,
    /// The material slice inside `bytes` (set by the reader).
    len: usize = 0,

    pub fn init(gpa: Allocator, capacity: usize) Allocator.Error!SecretBuf {
        const bytes = try gpa.alignedAlloc(u8, .fromByteUnits(4096), capacity);
        @memset(bytes, 0);
        var locked = false;
        if (is_linux) {
            // A failed mlock warns (a container with a tiny memlock cap):
            // the material still lives, un-pinned.
            const rc = linux.mlock(bytes.ptr, bytes.len);
            locked = linux.errno(rc) == .SUCCESS;
            if (!locked) log.warn("mlock of the key material buffer failed ({t}): the material may be swapped out", .{linux.errno(rc)});
            const ma = linux.madvise(bytes.ptr, bytes.len, linux.MADV.DONTDUMP);
            if (linux.errno(ma) != .SUCCESS) log.warn("madvise(MADV_DONTDUMP) of the key material buffer failed ({t})", .{linux.errno(ma)});
        }
        return .{ .gpa = gpa, .bytes = bytes, .locked = locked };
    }

    pub fn material(self: *const SecretBuf) []const u8 {
        return self.bytes[0..self.len];
    }

    /// Zeroize in place (the parent after a fork; every consumer when done).
    pub fn zeroize(self: *SecretBuf) void {
        std.crypto.secureZero(u8, self.bytes);
        self.len = 0;
    }

    pub fn deinit(self: *SecretBuf) void {
        self.zeroize();
        if (is_linux and self.locked) _ = linux.munlock(self.bytes.ptr, self.bytes.len);
        self.gpa.free(self.bytes);
        self.* = undefined;
    }
};

/// `/etc/squeezefs/keys/<16 hex>.key` (the resolution's rung 3).
pub fn keyPath(buf: *[128]u8, key_id: [transform.KEY_ID_LEN]u8) [:0]const u8 {
    return transform.defaultKeyPath(buf, key_id);
}

pub const linux_verbs = if (is_linux) struct {
    const cli = @import("../cli/cli.zig");
    const args = @import("../cli/args.zig");
    const uri = @import("../cli/uri.zig");
    const status = @import("../cli/status.zig");
    const uring_fs = @import("../io/uring_fs.zig");
    const derive = @import("../core/derive.zig");
    const knobs = @import("../core/knobs.zig");
    const set_config = @import("../data/set_config.zig");

    /// The material of `--encrypt-key <path|->` read into a pinned buffer
    /// (the pre-fork stash): the FD laws of `transform.linux_keys` apply.
    pub fn readStash(gpa: Allocator, flag_path: []const u8) (transform.KeyFileError || Allocator.Error)!SecretBuf {
        var buf = try SecretBuf.init(gpa, transform.MAX_KEY_FILE_BYTES + 1);
        errdefer buf.deinit();
        transform.linux_keys.hardenProcess();
        const m = if (std.mem.eql(u8, flag_path, "-")) try transform.linux_keys.readKeyStdin(buf.bytes) else try transform.linux_keys.readKeyFile(flag_path, buf.bytes);
        // The trimmed material sits inside `bytes`; move it to the front
        // so `material()` is a prefix and the rest zeroizes now.
        const off = @intFromPtr(m.ptr) - @intFromPtr(buf.bytes.ptr);
        std.mem.copyForwards(u8, buf.bytes[0..m.len], buf.bytes[off .. off + m.len]);
        std.crypto.secureZero(u8, buf.bytes[m.len..]);
        buf.len = m.len;
        return buf;
    }

    /// Derive against the set's salt and verify the recorded id; the
    /// stash is zeroized by the CALLER once the derived key is consumed.
    pub fn deriveFromStash(stash: *const SecretBuf, salt: *const [transform.KDF_SALT_LEN]u8, key_id: [transform.KEY_ID_LEN]u8) transform.KeyFileError!transform.DerivedKey {
        var derived = transform.deriveVolumeKey(stash.material(), salt);
        if (!std.mem.eql(u8, &derived.key_id, &key_id)) {
            derived.zeroize();
            return error.KeyMismatch;
        }
        return derived;
    }

    const Loaded = struct {
        cfg: set_config.Config,
        pool: *uring_fs.Pool,
        meta_path: []const u8,

        fn deinit(self: *Loaded, gpa: Allocator) void {
            self.cfg.deinit(gpa);
            self.pool.deinit();
        }
    };

    /// The set config read OFFLINE (the `config show` pattern).
    fn loadSet(p: cli.Process, verb: []const u8, meta_uri: []const u8) ?Loaded {
        var members: [uri.MAX_MEMBERS][]const u8 = undefined;
        var udiag: uri.Refusal = undefined;
        const meta = uri.parse(.meta, meta_uri, &members, &udiag) catch {
            p.stderr.print("squeezefs {s}: {f}\n", .{ verb, udiag }) catch {};
            return null;
        };
        const path = meta.first();
        const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
            p.stderr.print("squeezefs {s}: io pool init failed ({s})\n", .{ verb, @errorName(e) }) catch {};
            return null;
        };
        errdefer pool.deinit();
        var reason_buf: [512]u8 = undefined;
        const snap = status.classify(p.gpa, p.env, pool, path, &reason_buf) catch |e| {
            p.stderr.print("squeezefs {s}: cannot read {s}: {s}\n", .{ verb, path, @errorName(e) }) catch {};
            pool.deinit();
            return null;
        };
        const sb = snap.sb orelse {
            p.stderr.print("squeezefs {s}: {s} is not a formatted v4 metadata volume\n", .{ verb, path }) catch {};
            pool.deinit();
            return null;
        };
        var rec = (status.newestLedger(p.gpa, p.env, pool, path, sb) catch null) orelse {
            p.stderr.print("squeezefs {s}: {s} has no valid ledger record\n", .{ verb, path }) catch {};
            pool.deinit();
            return null;
        };
        defer rec.deinit(p.gpa);
        var cfg_reason: [512]u8 = undefined;
        const read = status.readSetConfig(p.gpa, p.io, p.env, pool, path, sb, &rec, &cfg_reason) catch |e| {
            p.stderr.print("squeezefs {s}: cannot read the control tree of {s}: {s}\n", .{ verb, path, @errorName(e) }) catch {};
            pool.deinit();
            return null;
        };
        switch (read) {
            .absent => {
                p.stderr.print("squeezefs {s}: {s} carries no set_config record — not member 0 of a set\n", .{ verb, path }) catch {};
                pool.deinit();
                return null;
            },
            .refused => |why| {
                p.stderr.print("squeezefs {s}: {s}: set config refused: {s}\n", .{ verb, path, why }) catch {};
                pool.deinit();
                return null;
            },
            .config => |cfg| return .{ .cfg = cfg, .pool = pool, .meta_path = path },
        }
    }

    fn requireEncrypted(p: cli.Process, verb: []const u8, cfg: *const set_config.Config) bool {
        if (cfg.encryption != .none) return true;
        p.stderr.print("squeezefs {s}: the set is plaintext (encryption none) — nothing would be encrypted; there is no key to manage\n", .{verb}) catch {};
        return false;
    }

    /// `key install <sqmeta-uri> <path|->`: read through the ONE reader,
    /// derive against the set's salt, REFUSE on an id mismatch, write the
    /// store file atomically (0600 root-owned `.tmp` → fdatasync → rename;
    /// the dir 0700); an identical existing file is a no-op, a different
    /// one refuses without `--force`.
    pub fn install(p: cli.Process, meta_uri: []const u8, source: []const u8, force: bool) u8 {
        var loaded = loadSet(p, "key install", meta_uri) orelse return cli.exit_failed;
        defer loaded.deinit(p.gpa);
        if (!requireEncrypted(p, "key install", &loaded.cfg)) return cli.exit_failed;
        var stash = readStash(p.gpa, source) catch |e| switch (e) {
            error.OutOfMemory => {
                p.stderr.writeAll("squeezefs key install: out of memory\n") catch {};
                return cli.exit_failed;
            },
            else => |ke| {
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                transform.writeKeyFileRefusal(&w, ke, source) catch {};
                p.stderr.print("squeezefs key install: {s}\n", .{w.buffered()}) catch {};
                return cli.exit_failed;
            },
        };
        defer stash.deinit();
        var derived = deriveFromStash(&stash, &loaded.cfg.kdf_salt, loaded.cfg.key_id) catch {
            p.stderr.print("squeezefs key install: {s} is not the key this set was formatted with (its derived key id differs from {x}) — refusing to install a wrong key\n", .{ source, &loaded.cfg.key_id }) catch {};
            return cli.exit_failed;
        };
        derived.zeroize();
        var pb: [128]u8 = undefined;
        const dest = keyPath(&pb, loaded.cfg.key_id);
        // An existing file: identical bytes ⇒ no-op; different ⇒ --force.
        var existing_buf: [transform.MAX_KEY_FILE_BYTES + 1]u8 = undefined;
        defer std.crypto.secureZero(u8, &existing_buf);
        if (transform.linux_keys.readKeyFile(dest, &existing_buf)) |existing| {
            if (std.mem.eql(u8, existing, stash.material())) {
                p.stdout.print("{s} already holds this key (id {x}) — nothing to do\n", .{ dest, &loaded.cfg.key_id }) catch {};
                p.stdout.flush() catch {};
                return cli.exit_ok;
            }
            if (!force) {
                p.stderr.print("squeezefs key install: {s} exists with DIFFERENT material — pass --force to replace it (the set's recorded id {x} matched the new material, so the old file is not this set's key)\n", .{ dest, &loaded.cfg.key_id }) catch {};
                return cli.exit_failed;
            }
        } else |e| switch (e) {
            error.KeyFileOpenFailed => {},
            else => {
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                transform.writeKeyFileRefusal(&w, e, dest) catch {};
                p.stderr.print("squeezefs key install: the existing {s} — {s}; move it aside\n", .{ dest, w.buffered() }) catch {};
                return cli.exit_failed;
            },
        }
        writeStoreFile(loaded.pool, dest, stash.material()) catch |e| {
            p.stderr.print("squeezefs key install: cannot write {s}: {s} (root, with {s} writable, is required)\n", .{ dest, @errorName(e), KEY_DIR_PARENT }) catch {};
            return cli.exit_failed;
        };
        p.stdout.print("installed {s} (id {x}, {d} bytes of material, mode 0600) — mounts of this set resolve it as rung 3 (after --encrypt-key and SQUEEZEFS_ENCRYPT_KEY_FILE)\n", .{ dest, &loaded.cfg.key_id, stash.material().len }) catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }

    pub const InstallOutcome = enum { installed, identical };
    pub const InstallError = error{ Exists, Unreadable, WriteFailed };

    /// The store file of `key_id` written from `material` (the rotation's
    /// next key, whose id the caller derived): identical bytes ⇒ no-op;
    /// different ⇒ `Exists` unless `force`.
    pub fn installMaterial(pool: *uring_fs.Pool, key_id: [transform.KEY_ID_LEN]u8, material: []const u8, force: bool) InstallError!InstallOutcome {
        var pb: [128]u8 = undefined;
        const dest = keyPath(&pb, key_id);
        var existing_buf: [transform.MAX_KEY_FILE_BYTES + 1]u8 = undefined;
        defer std.crypto.secureZero(u8, &existing_buf);
        if (transform.linux_keys.readKeyFile(dest, &existing_buf)) |existing| {
            if (std.mem.eql(u8, existing, material)) return .identical;
            if (!force) return error.Exists;
        } else |e| switch (e) {
            error.KeyFileOpenFailed => {},
            else => return error.Unreadable,
        }
        writeStoreFile(pool, dest, material) catch return error.WriteFailed;
        return .installed;
    }

    /// The rotation's NEXT key: the key dir alone (`--encrypt-key` and
    /// `SQUEEZEFS_ENCRYPT_KEY_FILE` name the primary), derived against the
    /// window's salt and verified against its id.
    pub fn resolveNextKey(salt: *const [transform.KDF_SALT_LEN]u8, key_id: [transform.KEY_ID_LEN]u8) transform.KeyFileError!transform.DerivedKey {
        var pb: [128]u8 = undefined;
        const path = keyPath(&pb, key_id);
        var material_buf: [transform.MAX_KEY_FILE_BYTES + 1]u8 = undefined;
        defer std.crypto.secureZero(u8, &material_buf);
        const m = try transform.linux_keys.readKeyFile(path, &material_buf);
        var derived = transform.deriveVolumeKey(m, salt);
        if (!std.mem.eql(u8, &derived.key_id, &key_id)) {
            derived.zeroize();
            return error.KeyMismatch;
        }
        return derived;
    }

    /// 0700 dirs, a 0600 `.tmp` created O_EXCL, the bytes through the
    /// io_uring file layer, fdatasync, rename, fsync(dir).
    fn writeStoreFile(pool: *uring_fs.Pool, dest: [:0]const u8, material: []const u8) !void {
        _ = linux.mkdir(KEY_DIR_PARENT, 0o700);
        _ = linux.mkdir(KEY_DIR, 0o700);
        try writeAtomic(pool, KEY_DIR, dest, material, 0o600);
    }

    /// `dest` inside `dir`: a `.tmp` created O_EXCL at `mode`, the bytes
    /// through the io_uring file layer, fdatasync, rename, fsync(dir).
    fn writeAtomic(pool: *uring_fs.Pool, dir: [:0]const u8, dest: [:0]const u8, bytes: []const u8, mode: linux.mode_t) !void {
        var tb: [linux.PATH_MAX]u8 = undefined;
        const tmp = try std.fmt.bufPrintSentinel(&tb, "{s}.tmp", .{dest}, 0);
        _ = linux.unlink(tmp);
        const rc = linux.open(tmp, .{ .ACCMODE = .WRONLY, .CREAT = true, .EXCL = true, .CLOEXEC = true }, mode);
        if (linux.errno(rc) != .SUCCESS) return @import("../core/errors.zig").fromErrno(linux.errno(rc));
        _ = linux.close(@intCast(rc));
        errdefer _ = linux.unlink(tmp);
        try pool.writeAt(tmp, 0, bytes);
        try pool.fdatasync(tmp);
        const rr = linux.renameat(linux.AT.FDCWD, tmp, linux.AT.FDCWD, dest);
        if (linux.errno(rr) != .SUCCESS) return @import("../core/errors.zig").fromErrno(linux.errno(rr));
        const drc = linux.open(dir, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(drc) == .SUCCESS) {
            const dfd: linux.fd_t = @intCast(drc);
            defer _ = linux.close(dfd);
            _ = linux.fsync(dfd);
        }
    }

    // ---- the cluster wire's PKI (`wire/pki.zig`; decision 0003 §P) ----

    const pki = @import("../wire/pki.zig");
    const wire_tls = @import("../wire/tls.zig");
    const clock = @import("../core/clock.zig");

    /// The default CA name when `--name` is absent.
    pub const DEFAULT_CA_NAME = "squeezefs-ca";
    /// A fresh certificate's `notBefore` is backdated this much so a peer
    /// whose clock lags by an NTP-grade offset still sees it valid at the
    /// first handshake (charter §5.5's membership skew is 500 ppm of
    /// T_owner ≈ 23 ms; five minutes covers an unsynchronised host).
    pub const NOT_BEFORE_SKEW_S: u64 = 5 * std.time.s_per_min;

    fn tlsPath(buf: *[linux.PATH_MAX]u8, dir: []const u8, name: []const u8) ![:0]const u8 {
        return std.fmt.bufPrintSentinel(buf, "{s}/{s}", .{ dir, name }, 0) catch error.NameTooLong;
    }

    fn dirZ(buf: *[linux.PATH_MAX]u8, dir: []const u8) ![:0]const u8 {
        return std.fmt.bufPrintSentinel(buf, "{s}", .{dir}, 0) catch error.NameTooLong;
    }

    fn fileExists(path: [:0]const u8) bool {
        var stx: linux.Statx = undefined;
        return linux.errno(linux.statx(linux.AT.FDCWD, path, linux.AT.SYMLINK_NOFOLLOW, .{ .TYPE = true }, &stx)) == .SUCCESS;
    }

    /// Write a PEM pair: the key 0600, the certificate 0644 (public; the
    /// loader's law is "nobody else may WRITE it").
    fn writePemPair(pool: *uring_fs.Pool, dir: [:0]const u8, cert_path: [:0]const u8, key_path: [:0]const u8, der: []const u8, kp: *const pki.KeyPair) !void {
        var text: [4096]u8 = undefined;
        var seed = pki.seedOf(kp);
        defer std.crypto.secureZero(u8, &seed);
        var pkcs8 = pki.encodePkcs8(&seed);
        defer std.crypto.secureZero(u8, &pkcs8);
        const key_pem = try pki.pemEncode(pki.PEM_PRIVATE_KEY, &pkcs8, &text);
        try writeAtomic(pool, dir, key_path, key_pem, 0o600);
        std.crypto.secureZero(u8, &text);
        const cert_pem = try pki.pemEncode(pki.PEM_CERTIFICATE, der, &text);
        try writeAtomic(pool, dir, cert_path, cert_pem, 0o644);
    }

    fn openPool(p: cli.Process, verb: []const u8) ?*uring_fs.Pool {
        return uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
            p.stderr.print("squeezefs {s}: io pool init failed ({s})\n", .{ verb, @errorName(e) }) catch {};
            return null;
        };
    }

    /// `key tls-init [--name NAME] [--force] <dir>`: the cluster CA.
    pub fn tlsInit(p: cli.Process, dir: []const u8, name: ?[]const u8, force: bool) u8 {
        const ca_name = name orelse DEFAULT_CA_NAME;
        if (!pki.nameValid(ca_name)) {
            p.stderr.print("squeezefs key tls-init: the CA name must be 1..{d} printable ASCII characters\n", .{pki.NAME_MAX}) catch {};
            return cli.exit_usage;
        }
        var db: [linux.PATH_MAX]u8 = undefined;
        var cb: [linux.PATH_MAX]u8 = undefined;
        var kb: [linux.PATH_MAX]u8 = undefined;
        const dz = dirZ(&db, dir) catch return cli.exit_usage;
        const cert_path = tlsPath(&cb, dir, "ca.pem") catch return cli.exit_usage;
        const key_path = tlsPath(&kb, dir, "ca.key") catch return cli.exit_usage;
        _ = linux.mkdir(dz, 0o700);
        if (fileExists(key_path) and !force) {
            p.stderr.print("squeezefs key tls-init: {s} exists — a CA key is never overwritten by accident; pass --force to mint a NEW CA (every node certificate under the old one stops verifying)\n", .{key_path}) catch {};
            return cli.exit_failed;
        }
        const now = clock.realtimeCoarseS();
        const ca = pki.mintCa(p.io, ca_name, now - NOT_BEFORE_SKEW_S) catch |e| {
            p.stderr.print("squeezefs key tls-init: cannot mint the CA ({s})\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        const pool = openPool(p, "key tls-init") orelse return cli.exit_failed;
        defer pool.deinit();
        writePemPair(pool, dz, cert_path, key_path, ca.der(), &ca.key) catch |e| {
            p.stderr.print("squeezefs key tls-init: cannot write {s}: {s}\n", .{ dir, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        p.stdout.print("minted CA '{s}' (Ed25519, valid {d} days): {s} (0644), {s} (0600) — issue node certificates with `squeezefs key tls-issue --node NAME {s}`\n", .{ ca_name, pki.CA_VALIDITY_S / std.time.s_per_day, cert_path, key_path, dir }) catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }

    /// `key tls-issue --node NAME [--out DIR] [--force] <ca-dir>`: a node
    /// certificate + key under the CA.
    pub fn tlsIssue(p: cli.Process, dir: []const u8, node: []const u8, out: ?[]const u8, force: bool) u8 {
        if (!pki.nameValid(node)) {
            p.stderr.print("squeezefs key tls-issue: the node name must be 1..{d} printable ASCII characters\n", .{pki.NAME_MAX}) catch {};
            return cli.exit_usage;
        }
        var cb: [linux.PATH_MAX]u8 = undefined;
        var kb: [linux.PATH_MAX]u8 = undefined;
        const ca_cert_path = tlsPath(&cb, dir, "ca.pem") catch return cli.exit_usage;
        const ca_key_path = tlsPath(&kb, dir, "ca.key") catch return cli.exit_usage;
        // The CA under the file laws (the key 0600, the cert nobody else writes).
        var ca_text: [transform.MAX_KEY_FILE_BYTES]u8 = undefined;
        const ca_pem = wire_tls.linux_files.readTrustFile(ca_cert_path, &ca_text) catch |e| {
            p.stderr.print("squeezefs key tls-issue: cannot read the CA certificate {s} ({s}; a regular file owned by you or root that nobody else may write)\n", .{ ca_cert_path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        var ca_der: [pki.CERT_MAX_DER]u8 = undefined;
        const ca_parsed = pki.parseCertificatePem(ca_pem, &ca_der) catch {
            p.stderr.print("squeezefs key tls-issue: {s} is not a PEM Ed25519 certificate\n", .{ca_cert_path}) catch {};
            return cli.exit_failed;
        };
        var key_text: [transform.MAX_KEY_FILE_BYTES + 1]u8 = undefined;
        defer std.crypto.secureZero(u8, &key_text);
        const key_pem = transform.linux_keys.readKeyFile(ca_key_path, &key_text) catch |e| {
            var buf: [512]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            transform.writeKeyFileRefusal(&w, e, ca_key_path) catch {};
            p.stderr.print("squeezefs key tls-issue: {s}\n", .{w.buffered()}) catch {};
            return cli.exit_failed;
        };
        const ca_key = pki.parsePrivateKeyPem(key_pem) catch {
            p.stderr.print("squeezefs key tls-issue: {s} is not a PEM PKCS#8 Ed25519 private key\n", .{ca_key_path}) catch {};
            return cli.exit_failed;
        };
        if (!std.mem.eql(u8, ca_parsed.publicKey(), &ca_key.public_key.toBytes())) {
            p.stderr.print("squeezefs key tls-issue: {s} is not the key of {s}\n", .{ ca_key_path, ca_cert_path }) catch {};
            return cli.exit_failed;
        }
        var ca_issued: pki.Issued = .{ .der_buf = undefined, .der_len = ca_parsed.der.len, .key = ca_key };
        @memcpy(ca_issued.der_buf[0..ca_parsed.der.len], ca_parsed.der);
        const out_dir = out orelse dir;
        var db: [linux.PATH_MAX]u8 = undefined;
        var ncb: [linux.PATH_MAX]u8 = undefined;
        var nkb: [linux.PATH_MAX]u8 = undefined;
        const dz = dirZ(&db, out_dir) catch return cli.exit_usage;
        const cert_path = tlsPath(&ncb, out_dir, "node.pem") catch return cli.exit_usage;
        const key_path = tlsPath(&nkb, out_dir, "node.key") catch return cli.exit_usage;
        _ = linux.mkdir(dz, 0o700);
        if (fileExists(key_path) and !force) {
            p.stderr.print("squeezefs key tls-issue: {s} exists — pass --force to replace the node's key and certificate\n", .{key_path}) catch {};
            return cli.exit_failed;
        }
        const now = clock.realtimeCoarseS();
        const issued = pki.mintNode(p.io, &ca_issued, ca_parsed.commonName(), node, now - NOT_BEFORE_SKEW_S, pki.NODE_VALIDITY_S) catch |e| {
            p.stderr.print("squeezefs key tls-issue: cannot issue the certificate ({s})\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        const pool = openPool(p, "key tls-issue") orelse return cli.exit_failed;
        defer pool.deinit();
        writePemPair(pool, dz, cert_path, key_path, issued.der(), &issued.key) catch |e| {
            p.stderr.print("squeezefs key tls-issue: cannot write {s}: {s}\n", .{ out_dir, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        p.stdout.print("issued node certificate CN='{s}' under CA '{s}' (Ed25519, valid {d} days): {s} (0644), {s} (0600) — point SQUEEZEFS_WIRE_TLS_CERT/_KEY at them (or install them as {s}/node.pem and node.key) and set SQUEEZEFS_WIRE_CHANNEL=mutual_tls\n", .{ node, ca_parsed.commonName(), pki.NODE_VALIDITY_S / std.time.s_per_day, cert_path, key_path, wire_tls.TLS_DIR }) catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }

    /// `key show <sqmeta-uri>`: scheme, key id, salt hex, and WHICH source
    /// would resolve now — never material, never the KEK (the derivation
    /// runs for the verdict and its result is zeroized at once).
    pub fn show(p: cli.Process, meta_uri: []const u8) u8 {
        var loaded = loadSet(p, "key show", meta_uri) orelse return cli.exit_failed;
        defer loaded.deinit(p.gpa);
        const cfg = &loaded.cfg;
        if (cfg.encryption == .none) {
            p.stdout.print("{s}: plaintext set (encryption none) — no key\n", .{meta_uri}) catch {};
            p.stdout.flush() catch {};
            return cli.exit_ok;
        }
        var pb: [128]u8 = undefined;
        p.stdout.print("{s}:\n  encryption {s}; key wrap {s} (scheme {d}); key id {x}\n  kdf salt {x}\n  resolution: --encrypt-key → SQUEEZEFS_ENCRYPT_KEY_FILE → {s}\n", .{ meta_uri, cfg.encryption.name(), transform.KEY_WRAP_SCHEME_NAME, cfg.key_scheme, &cfg.key_id, &cfg.kdf_salt, keyPath(&pb, cfg.key_id) }) catch {};
        var path_buf: [128]u8 = undefined;
        var served: []const u8 = "";
        if (transform.linux_keys.resolveVolumeKey(p.env, null, &cfg.kdf_salt, cfg.key_id, &path_buf, &served)) |d| {
            var dd = d;
            dd.zeroize();
            p.stdout.print("  resolves now from {s} ({s}): the derived id MATCHES\n", .{ served, if (knobs.present(p.env.get("SQUEEZEFS_ENCRYPT_KEY_FILE")) != null) "SQUEEZEFS_ENCRYPT_KEY_FILE" else "the key store" }) catch {};
        } else |e| {
            var buf: [512]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            transform.writeKeyFileRefusal(&w, e, if (served.len != 0) served else keyPath(&path_buf, cfg.key_id)) catch {};
            p.stdout.print("  resolves now: NO — {s}\n", .{w.buffered()}) catch {};
        }
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }

    /// `key check <sqmeta-uri> [--encrypt-key <path|->]`: the mount's
    /// resolution without a mount; exit 0 iff the id matches.
    pub fn check(p: cli.Process, meta_uri: []const u8, flag_path: ?[]const u8) u8 {
        var loaded = loadSet(p, "key check", meta_uri) orelse return cli.exit_failed;
        defer loaded.deinit(p.gpa);
        if (!requireEncrypted(p, "key check", &loaded.cfg)) return cli.exit_failed;
        var path_buf: [128]u8 = undefined;
        var served: []const u8 = "";
        var d = transform.linux_keys.resolveVolumeKey(p.env, flag_path, &loaded.cfg.kdf_salt, loaded.cfg.key_id, &path_buf, &served) catch |e| {
            var buf: [512]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            transform.writeKeyFileRefusal(&w, e, if (served.len != 0) served else keyPath(&path_buf, loaded.cfg.key_id)) catch {};
            p.stderr.print("squeezefs key check: {s}\n", .{w.buffered()}) catch {};
            return cli.exit_failed;
        };
        d.zeroize();
        p.stdout.print("key ok: {s} derives the recorded id {x}\n", .{ served, &loaded.cfg.key_id }) catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }

    pub fn run(p: cli.Process, k: args.Key) u8 {
        return switch (k) {
            .install => |i| install(p, i.meta_uri, i.source, i.force),
            .show => |s| show(p, s.meta_uri),
            .check => |c| check(p, c.meta_uri, c.encrypt_key),
            .tls_init => |t| tlsInit(p, t.dir, t.name, t.force),
            .tls_issue => |t| tlsIssue(p, t.dir, t.node, t.out, t.force),
        };
    }
} else struct {};

const testing = std.testing;

test "SecretBuf: zeroized on deinit whatever happened; the material slice is a prefix; a stash read off a 0600 file is trimmed and pinned, a 0644 file refuses (the FD laws)" {
    const gpa = testing.allocator;
    var b = try SecretBuf.init(gpa, 4096);
    @memcpy(b.bytes[0..4], "abcd");
    b.len = 4;
    try testing.expectEqualStrings("abcd", b.material());
    const ptr = b.bytes;
    b.zeroize();
    try testing.expect(std.mem.allEqual(u8, ptr, 0));
    try testing.expectEqual(@as(usize, 0), b.len);
    b.deinit();
    if (!is_linux) return;
    _ = linux.mkdir("/build/tmp", 0o755);
    // Pid-tagged: two test processes on the venue never share the stash.
    var path_buf: [64]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&path_buf, "/build/tmp/keys-stash-test-{d}.key", .{linux.getpid()}, 0);
    {
        const rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        try testing.expect(linux.errno(rc) == .SUCCESS);
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        const text = "  0123456789abcdef0123456789abcdef0123456789abcdef  \n";
        _ = linux.write(fd, text, text.len);
    }
    defer _ = linux.unlink(path.ptr);
    var s = try linux_verbs.readStash(gpa, path);
    defer s.deinit();
    try testing.expectEqualStrings("0123456789abcdef0123456789abcdef0123456789abcdef", s.material());
    try testing.expect(std.mem.allEqual(u8, s.bytes[s.len..], 0));
    var salt: [transform.KDF_SALT_LEN]u8 = @splat(3);
    var d = transform.deriveVolumeKey(s.material(), &salt);
    defer d.zeroize();
    var ok = try linux_verbs.deriveFromStash(&s, &salt, d.key_id);
    ok.zeroize();
    try testing.expectError(error.KeyMismatch, linux_verbs.deriveFromStash(&s, &salt, @splat(0)));
    _ = linux.chmod(path.ptr, 0o644);
    try testing.expectError(error.KeyFilePermissions, linux_verbs.readStash(gpa, path));
    var pb: [128]u8 = undefined;
    try testing.expectEqualStrings("/etc/squeezefs/keys/0102030405060708.key", keyPath(&pb, .{ 1, 2, 3, 4, 5, 6, 7, 8 }));
}
