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

    /// 0700 dirs, a 0600 `.tmp` created O_EXCL, the bytes through the
    /// io_uring file layer, fdatasync, rename, fsync(dir).
    fn writeStoreFile(pool: *uring_fs.Pool, dest: [:0]const u8, material: []const u8) !void {
        _ = linux.mkdir(KEY_DIR_PARENT, 0o700);
        _ = linux.mkdir(KEY_DIR, 0o700);
        var tb: [160]u8 = undefined;
        const tmp = try std.fmt.bufPrintSentinel(&tb, "{s}.tmp", .{dest}, 0);
        _ = linux.unlink(tmp);
        const rc = linux.open(tmp, .{ .ACCMODE = .WRONLY, .CREAT = true, .EXCL = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return @import("../core/errors.zig").fromErrno(linux.errno(rc));
        _ = linux.close(@intCast(rc));
        errdefer _ = linux.unlink(tmp);
        try pool.writeAt(tmp, 0, material);
        try pool.fdatasync(tmp);
        const rr = linux.renameat(linux.AT.FDCWD, tmp, linux.AT.FDCWD, dest);
        if (linux.errno(rr) != .SUCCESS) return @import("../core/errors.zig").fromErrno(linux.errno(rr));
        const drc = linux.open(KEY_DIR, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(drc) == .SUCCESS) {
            const dfd: linux.fd_t = @intCast(drc);
            defer _ = linux.close(dfd);
            _ = linux.fsync(dfd);
        }
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
    const path = "/build/tmp/keys-stash-test.key";
    {
        const rc = linux.open(path, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        try testing.expect(linux.errno(rc) == .SUCCESS);
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        const text = "  0123456789abcdef0123456789abcdef0123456789abcdef  \n";
        _ = linux.write(fd, text, text.len);
    }
    defer _ = linux.unlink(path);
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
    _ = linux.chmod(path, 0o644);
    try testing.expectError(error.KeyFilePermissions, linux_verbs.readStash(gpa, path));
    var pb: [128]u8 = undefined;
    try testing.expectEqualStrings("/etc/squeezefs/keys/0102030405060708.key", keyPath(&pb, .{ 1, 2, 3, 4, 5, 6, 7, 8 }));
}
