//! The CLI grammar (charter §8): `format`, `mount`, `umount`, `status`,
//! `--version`, `--help`, parsed into ONE typed `Verb`. Flags and
//! positionals mix freely; `--` ends the flags; a value rides `--flag VALUE`
//! or `--flag=VALUE` (`-o OPTS` / `-oOPTS`). Every integer goes through the
//! ONE knob parser (`knobs.parseIntIn`: inclusive range, refuse never clamp)
//! and every URI through the ONE URI parser (`uri.zig`). A refusal is typed
//! and formats as one line naming the offending word and the fix; the
//! caller prints it with the verb's usage and exits 2.
//!
//! Portable; pure over argv slices (the `Verb` borrows them).

const std = @import("std");
const knobs = @import("../core/knobs.zig");
const meta_geometry = @import("../meta/geometry.zig");
const uri = @import("uri.zig");

/// `--meta-journal-mb`: the ring override in MiB. Floor 1 MiB: the plan's
/// own floor is 384 KiB (`geometry.journal_override_floor`) and a whole
/// MiB is the unit the flag names. Ceiling 1024: the derived clamp tops at
/// 32 MiB on a 2 GiB+ volume (charter §5.1), so an override past 1 GiB is
/// 32× any derivation — a typo, never a posture.
pub const JOURNAL_MB_MIN: u32 = 1;
pub const JOURNAL_MB_MAX: u32 = 1024;
/// `--meta-node-kib`: the five powers of two `geometry.validateNodeKib`
/// admits; the integer range brackets them so the ONE parser's message
/// names a sane window before the exact-set check names the five.
pub const NODE_KIB_MIN: u32 = meta_geometry.allowed_node_kib[0];
pub const NODE_KIB_MAX: u32 = meta_geometry.allowed_node_kib[meta_geometry.allowed_node_kib.len - 1];

pub const Format = struct {
    /// Raw `sqmeta://` word (parsed by `uri`; kept raw so the verb can
    /// re-print it).
    meta_uri: []const u8,
    /// Raw `sqdata://` words, in order.
    data_uris: []const []const u8,
    single_writer: bool = false,
    /// Bytes; null = the shipped default node.
    node_size: ?u64 = null,
    /// Bytes; null = the derived clamp.
    journal_len: ?u64 = null,
    force: bool = false,
};

pub const Mount = struct {
    meta_uri: []const u8,
    mountpoint: []const u8,
    /// The raw `-o` string (the mount verb parses it over the env TTLs).
    options: ?[]const u8 = null,
    daemon: bool = false,
    supervise: bool = false,
    log_file: ?[]const u8 = null,
};

pub const Umount = struct {
    mountpoint: []const u8,
};

pub const Status = struct {
    meta_uri: []const u8,
};

pub const Verb = union(enum) {
    version,
    help,
    format: Format,
    mount: Mount,
    umount: Umount,
    status: Status,
};

pub const Refusal = union(enum) {
    no_verb,
    unknown_verb: []const u8,
    unknown_flag: struct { verb: []const u8, flag: []const u8 },
    missing_value: struct { verb: []const u8, flag: []const u8 },
    /// An integer flag through the ONE parser.
    bad_int: knobs.Diagnostic,
    bad_node_kib: u32,
    bad_uri: uri.Refusal,
    missing_positional: struct { verb: []const u8, what: []const u8 },
    extra_positional: struct { verb: []const u8, word: []const u8 },
    supervise_without_daemon,
    log_file_without_daemon,
    daemon_without_log_file,

    pub fn format(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .no_verb => try w.writeAll("no verb given"),
            .unknown_verb => |v| try w.print("'{s}' is not a squeezefs verb", .{v}),
            .unknown_flag => |f| try w.print("{s}: unknown flag '{s}'", .{ f.verb, f.flag }),
            .missing_value => |f| try w.print("{s}: '{s}' requires a value", .{ f.verb, f.flag }),
            .bad_int => |d| try w.print("{f}", .{d}),
            .bad_node_kib => |kib| try meta_geometry.writeNodeKibRefusal(w, kib),
            .bad_uri => |r| try w.print("{f}", .{r}),
            .missing_positional => |m| try w.print("{s}: missing {s}", .{ m.verb, m.what }),
            .extra_positional => |e| try w.print("{s}: unexpected argument '{s}'", .{ e.verb, e.word }),
            .supervise_without_daemon => try w.writeAll("mount: --supervise requires --daemon (the supervisor is the daemon's parent process)"),
            .log_file_without_daemon => try w.writeAll("mount: --log-file requires --daemon (a foreground mount logs to stderr)"),
            .daemon_without_log_file => try w.writeAll("mount: --daemon requires --log-file PATH (a detached daemon must log somewhere; the invoking shell's streams are released at ready)"),
        }
    }
};

pub const Error = error{Refused};

/// A verb's usage, one line per form.
pub fn usage(verb: ?std.meta.Tag(Verb)) []const u8 {
    const all =
        \\usage: squeezefs <verb> [flags] …
        \\  squeezefs format [--single-writer] [--meta-node-kib N] [--meta-journal-mb N] [--force] sqmeta://<path> [sqdata://<path>…]
        \\  squeezefs mount [-o opt[,opt…]] [--daemon --log-file PATH [--supervise]] sqmeta://<path> <mountpoint>
        \\  squeezefs umount <mountpoint>
        \\  squeezefs status sqmeta://<path>
        \\  squeezefs --version
        \\
    ;
    const v = verb orelse return all;
    return switch (v) {
        .format => "usage: squeezefs format [--single-writer] [--meta-node-kib 64|128|256|512|1024] [--meta-journal-mb N] [--force] sqmeta://<path> [sqdata://<path>…]\n",
        .mount => "usage: squeezefs mount [-o opt[,opt…]] [--daemon --log-file PATH [--supervise]] sqmeta://<path> <mountpoint>\n",
        .umount => "usage: squeezefs umount <mountpoint>\n",
        .status => "usage: squeezefs status sqmeta://<path>\n",
        .version, .help => all,
    };
}

/// A flag word's name and inline value (`--k=v` / `-ov`).
const Flag = struct {
    name: []const u8,
    inline_value: ?[]const u8,

    fn of(word: []const u8) ?Flag {
        if (word.len < 2 or word[0] != '-') return null;
        if (std.mem.startsWith(u8, word, "--")) {
            if (std.mem.indexOfScalar(u8, word, '=')) |eq| return .{ .name = word[0..eq], .inline_value = word[eq + 1 ..] };
            return .{ .name = word, .inline_value = null };
        }
        // A short flag: `-o` alone or `-o<value>` glued.
        return .{ .name = word[0..2], .inline_value = if (word.len > 2) word[2..] else null };
    }
};

/// Cursor over `argv[2..]` with the shared value/positional mechanics.
const Cursor = struct {
    verb: []const u8,
    words: []const []const u8,
    i: usize = 0,
    flags_ended: bool = false,
    positionals: [uri.MAX_MEMBERS + 2][]const u8 = undefined,
    npos: usize = 0,
    diag: *Refusal,

    fn next(self: *Cursor) ?[]const u8 {
        if (self.i >= self.words.len) return null;
        const w = self.words[self.i];
        self.i += 1;
        return w;
    }

    /// The flag's value: inline, else the next word.
    fn value(self: *Cursor, f: Flag) Error![]const u8 {
        if (f.inline_value) |v| return v;
        return self.next() orelse {
            self.diag.* = .{ .missing_value = .{ .verb = self.verb, .flag = f.name } };
            return error.Refused;
        };
    }

    fn positional(self: *Cursor, word: []const u8) Error!void {
        if (self.npos == self.positionals.len) {
            self.diag.* = .{ .extra_positional = .{ .verb = self.verb, .word = word } };
            return error.Refused;
        }
        self.positionals[self.npos] = word;
        self.npos += 1;
    }

    fn unknown(self: *Cursor, f: Flag) Error {
        self.diag.* = .{ .unknown_flag = .{ .verb = self.verb, .flag = f.name } };
        return error.Refused;
    }

    /// Classify the next word: a flag (to the verb's switch), a positional
    /// (collected), or the end.
    fn step(self: *Cursor) Error!?Flag {
        while (self.next()) |w| {
            if (!self.flags_ended) {
                if (std.mem.eql(u8, w, "--")) {
                    self.flags_ended = true;
                    continue;
                }
                if (Flag.of(w)) |f| return f;
            }
            try self.positional(w);
        }
        return null;
    }

    fn pos(self: *Cursor) []const []const u8 {
        return self.positionals[0..self.npos];
    }
};

fn parseIntFlag(c: *Cursor, f: Flag, comptime T: type, lo: T, hi: T) Error!T {
    const raw = try c.value(f);
    return switch (knobs.parseIntIn(T, f.name, raw, lo, hi)) {
        .value => |v| v,
        .absent => blk: {
            c.diag.* = .{ .missing_value = .{ .verb = c.verb, .flag = f.name } };
            break :blk error.Refused;
        },
        .invalid => |d| blk: {
            c.diag.* = .{ .bad_int = d };
            break :blk error.Refused;
        },
    };
}

fn checkUri(scheme: uri.Scheme, word: []const u8, diag: *Refusal) Error!void {
    var buf: [uri.MAX_MEMBERS][]const u8 = undefined;
    var r: uri.Refusal = undefined;
    _ = uri.parse(scheme, word, &buf, &r) catch {
        diag.* = .{ .bad_uri = r };
        return error.Refused;
    };
}

/// `argv` as the process received it (`argv[0]` is the binary).
pub fn parse(argv: []const []const u8, diag: *Refusal) Error!Verb {
    if (argv.len < 2) {
        diag.* = .no_verb;
        return error.Refused;
    }
    const verb = argv[1];
    const words = argv[2..];
    if (std.mem.eql(u8, verb, "--version") or std.mem.eql(u8, verb, "version")) return .version;
    if (std.mem.eql(u8, verb, "--help") or std.mem.eql(u8, verb, "-h") or std.mem.eql(u8, verb, "help")) return .help;
    if (std.mem.eql(u8, verb, "format")) return parseFormat(words, diag);
    if (std.mem.eql(u8, verb, "mount")) return parseMount(words, diag);
    if (std.mem.eql(u8, verb, "umount")) return parseUmount(words, diag);
    if (std.mem.eql(u8, verb, "status")) return parseStatus(words, diag);
    diag.* = .{ .unknown_verb = verb };
    return error.Refused;
}

fn parseFormat(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "format", .words = words, .diag = diag };
    var out: Format = .{ .meta_uri = undefined, .data_uris = &.{} };
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--single-writer")) {
            out.single_writer = true;
        } else if (std.mem.eql(u8, f.name, "--force")) {
            out.force = true;
        } else if (std.mem.eql(u8, f.name, "--meta-node-kib")) {
            const kib = try parseIntFlag(&c, f, u32, NODE_KIB_MIN, NODE_KIB_MAX);
            var warn_buf: [256]u8 = undefined;
            const nk = meta_geometry.validateNodeKib(kib, &warn_buf) catch {
                diag.* = .{ .bad_node_kib = kib };
                return error.Refused;
            };
            out.node_size = nk.bytes;
        } else if (std.mem.eql(u8, f.name, "--meta-journal-mb")) {
            out.journal_len = @as(u64, try parseIntFlag(&c, f, u32, JOURNAL_MB_MIN, JOURNAL_MB_MAX)) << 20;
        } else return c.unknown(f);
    }
    // Positionals: exactly one sqmeta://; the sqdata:// words are the TAIL
    // of argv (one spelling: `… sqmeta://m sqdata://a sqdata://b`), so the
    // verb can borrow them as one slice of the caller's argv.
    var meta: ?[]const u8 = null;
    for (c.pos()) |w| {
        switch (uri.schemeOf(w) orelse {
            diag.* = .{ .bad_uri = .{ .wrong_scheme = .{ .want = .meta, .got = w } } };
            return error.Refused;
        }) {
            .meta => {
                if (meta != null) {
                    diag.* = .{ .extra_positional = .{ .verb = "format", .word = w } };
                    return error.Refused;
                }
                try checkUri(.meta, w, diag);
                meta = w;
            },
            .data => {
                if (meta == null) {
                    diag.* = .{ .missing_positional = .{ .verb = "format", .what = "the sqmeta:// volume (it precedes every sqdata:// volume)" } };
                    return error.Refused;
                }
            },
        }
    }
    out.meta_uri = meta orelse {
        diag.* = .{ .missing_positional = .{ .verb = "format", .what = "the sqmeta:// volume" } };
        return error.Refused;
    };
    out.data_uris = try dataTail(words, diag);
    return .{ .format = out };
}

/// The `sqdata://` words as the tail of argv: from the first one on, every
/// word must be a valid data URI (a flag or another positional after it is
/// refused by name).
fn dataTail(words: []const []const u8, diag: *Refusal) Error![]const []const u8 {
    const first = for (words, 0..) |w, i| {
        if (uri.schemeOf(w) == .data) break i;
    } else return &.{};
    for (words[first..]) |w| {
        if (uri.schemeOf(w) != .data) {
            diag.* = .{ .extra_positional = .{ .verb = "format", .word = w } };
            return error.Refused;
        }
        try checkUri(.data, w, diag);
    }
    return words[first..];
}

fn parseMount(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "mount", .words = words, .diag = diag };
    var out: Mount = .{ .meta_uri = undefined, .mountpoint = undefined };
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "-o")) {
            out.options = try c.value(f);
        } else if (std.mem.eql(u8, f.name, "--daemon")) {
            out.daemon = true;
        } else if (std.mem.eql(u8, f.name, "--supervise")) {
            out.supervise = true;
        } else if (std.mem.eql(u8, f.name, "--log-file")) {
            out.log_file = try c.value(f);
        } else return c.unknown(f);
    }
    const pos = c.pos();
    if (pos.len < 1) {
        diag.* = .{ .missing_positional = .{ .verb = "mount", .what = "the sqmeta:// volume" } };
        return error.Refused;
    }
    try checkUri(.meta, pos[0], diag);
    if (pos.len < 2) {
        diag.* = .{ .missing_positional = .{ .verb = "mount", .what = "the mountpoint" } };
        return error.Refused;
    }
    if (pos.len > 2) {
        diag.* = .{ .extra_positional = .{ .verb = "mount", .word = pos[2] } };
        return error.Refused;
    }
    out.meta_uri = pos[0];
    out.mountpoint = pos[1];
    if (out.supervise and !out.daemon) {
        diag.* = .supervise_without_daemon;
        return error.Refused;
    }
    if (out.log_file != null and !out.daemon) {
        diag.* = .log_file_without_daemon;
        return error.Refused;
    }
    if (out.daemon and out.log_file == null) {
        diag.* = .daemon_without_log_file;
        return error.Refused;
    }
    return .{ .mount = out };
}

fn parseUmount(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "umount", .words = words, .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const pos = c.pos();
    if (pos.len < 1) {
        diag.* = .{ .missing_positional = .{ .verb = "umount", .what = "the mountpoint" } };
        return error.Refused;
    }
    if (pos.len > 1) {
        diag.* = .{ .extra_positional = .{ .verb = "umount", .word = pos[1] } };
        return error.Refused;
    }
    return .{ .umount = .{ .mountpoint = pos[0] } };
}

fn parseStatus(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "status", .words = words, .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const pos = c.pos();
    if (pos.len < 1) {
        diag.* = .{ .missing_positional = .{ .verb = "status", .what = "the sqmeta:// volume" } };
        return error.Refused;
    }
    if (pos.len > 1) {
        diag.* = .{ .extra_positional = .{ .verb = "status", .word = pos[1] } };
        return error.Refused;
    }
    try checkUri(.meta, pos[0], diag);
    return .{ .status = .{ .meta_uri = pos[0] } };
}

// ---------------------------------------------------------------------------
// Tests: the grammar's contract.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn av(comptime words: []const []const u8) []const []const u8 {
    return &([_][]const u8{"squeezefs"} ++ words.*);
}

fn refusal(words: []const []const u8) !Refusal {
    var d: Refusal = undefined;
    try testing.expectError(error.Refused, parse(words, &d));
    return d;
}

fn text(buf: []u8, r: Refusal) []const u8 {
    return std.fmt.bufPrint(buf, "{f}", .{r}) catch unreachable;
}

test "version and help" {
    var d: Refusal = undefined;
    try testing.expectEqual(Verb.version, try parse(av(&.{"--version"}), &d));
    try testing.expectEqual(Verb.version, try parse(av(&.{"version"}), &d));
    try testing.expectEqual(Verb.help, try parse(av(&.{"--help"}), &d));
    try testing.expectEqual(Verb.help, try parse(av(&.{"-h"}), &d));
    try testing.expect((try refusal(av(&.{}))) == .no_verb);
    try testing.expect((try refusal(av(&.{"frobnicate"}))) == .unknown_verb);
}

test "format: flags in any order, --k=v and --k v, --meta-node-kib validated against the five, journal MiB → bytes, data URIs are the argv tail" {
    var d: Refusal = undefined;
    const v = try parse(av(&.{ "format", "--force", "--meta-node-kib=64", "--meta-journal-mb", "4", "--single-writer", "sqmeta:///build/tmp/m.img", "sqdata:///dev/a", "sqdata:///dev/b,/dev/c" }), &d);
    try testing.expectEqualStrings("sqmeta:///build/tmp/m.img", v.format.meta_uri);
    try testing.expect(v.format.force and v.format.single_writer);
    try testing.expectEqual(@as(?u64, 65536), v.format.node_size);
    try testing.expectEqual(@as(?u64, 4 << 20), v.format.journal_len);
    try testing.expectEqual(@as(usize, 2), v.format.data_uris.len);
    try testing.expectEqualStrings("sqdata:///dev/b,/dev/c", v.format.data_uris[1]);
    const plain = try parse(av(&.{ "format", "sqmeta:///dev/nvme0n1" }), &d);
    try testing.expect(!plain.format.force and plain.format.node_size == null and plain.format.journal_len == null and plain.format.data_uris.len == 0);
    // `--` ends the flags.
    const dashed = try parse(av(&.{ "format", "--", "sqmeta:///x" }), &d);
    try testing.expectEqualStrings("sqmeta:///x", dashed.format.meta_uri);
}

test "format refusals: a sixth node size, an out-of-range journal, junk integers, two metadata URIs, a relative path, a data URI before the metadata one or followed by a flag, no URI" {
    var buf: [512]u8 = undefined;
    var r = try refusal(av(&.{ "format", "--meta-node-kib", "96", "sqmeta:///x" }));
    try testing.expect(r == .bad_node_kib);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "64|128|256|512|1024") != null);
    r = try refusal(av(&.{ "format", "--meta-node-kib", "32", "sqmeta:///x" }));
    try testing.expect(r == .bad_int);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "64..=1024") != null);
    r = try refusal(av(&.{ "format", "--meta-journal-mb=0", "sqmeta:///x" }));
    try testing.expect(r == .bad_int);
    r = try refusal(av(&.{ "format", "--meta-journal-mb", "4MiB", "sqmeta:///x" }));
    try testing.expect(r == .bad_int);
    r = try refusal(av(&.{ "format", "--meta-journal-mb" }));
    try testing.expect(r == .missing_value);
    r = try refusal(av(&.{ "format", "sqmeta:///x", "sqmeta:///y" }));
    try testing.expect(r == .extra_positional);
    r = try refusal(av(&.{ "format", "sqmeta://x" }));
    try testing.expect(r == .bad_uri);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "must be absolute") != null);
    r = try refusal(av(&.{ "format", "sqdata:///d", "sqmeta:///x" }));
    try testing.expect(r == .missing_positional);
    r = try refusal(av(&.{ "format", "sqmeta:///x", "sqdata:///d", "--force" }));
    try testing.expect(r == .extra_positional);
    r = try refusal(av(&.{ "format", "sqmeta:///x", "sqdata://d" }));
    try testing.expect(r == .bad_uri);
    r = try refusal(av(&.{ "format", "--force" }));
    try testing.expect(r == .missing_positional);
    r = try refusal(av(&.{ "format", "/dev/nvme0n1" }));
    try testing.expect(r == .bad_uri);
    r = try refusal(av(&.{ "format", "--full", "sqmeta:///x" }));
    try testing.expect(r == .unknown_flag);
}

test "mount: -o glued or spaced, --daemon/--supervise/--log-file, two positionals in order" {
    var d: Refusal = undefined;
    const v = try parse(av(&.{ "mount", "-oallow_other,attr_timeout=2", "--daemon", "--supervise", "--log-file=/var/log/sqz.log", "sqmeta:///dev/m", "/mnt/sqz" }), &d);
    try testing.expectEqualStrings("allow_other,attr_timeout=2", v.mount.options.?);
    try testing.expect(v.mount.daemon and v.mount.supervise);
    try testing.expectEqualStrings("/var/log/sqz.log", v.mount.log_file.?);
    try testing.expectEqualStrings("sqmeta:///dev/m", v.mount.meta_uri);
    try testing.expectEqualStrings("/mnt/sqz", v.mount.mountpoint);
    const fg = try parse(av(&.{ "mount", "sqmeta:///dev/m", "/mnt/sqz", "-o", "ro" }), &d);
    try testing.expectEqualStrings("ro", fg.mount.options.?);
    try testing.expect(!fg.mount.daemon and fg.mount.log_file == null);
}

test "mount refusals: supervise/log-file without daemon, daemon without a log file, a missing mountpoint, a third positional, a bad URI, an unknown flag" {
    var r = try refusal(av(&.{ "mount", "--supervise", "sqmeta:///m", "/mnt" }));
    try testing.expect(r == .supervise_without_daemon);
    r = try refusal(av(&.{ "mount", "--log-file", "/l", "sqmeta:///m", "/mnt" }));
    try testing.expect(r == .log_file_without_daemon);
    r = try refusal(av(&.{ "mount", "--daemon", "sqmeta:///m", "/mnt" }));
    try testing.expect(r == .daemon_without_log_file);
    r = try refusal(av(&.{ "mount", "sqmeta:///m" }));
    try testing.expect(r == .missing_positional);
    r = try refusal(av(&.{ "mount", "sqmeta:///m", "/mnt", "extra" }));
    try testing.expect(r == .extra_positional);
    r = try refusal(av(&.{ "mount", "/dev/m", "/mnt" }));
    try testing.expect(r == .bad_uri);
    r = try refusal(av(&.{ "mount", "--fg", "sqmeta:///m", "/mnt" }));
    try testing.expect(r == .unknown_flag);
    r = try refusal(av(&.{ "mount", "-o" }));
    try testing.expect(r == .missing_value);
}

test "umount and status take exactly one positional" {
    var d: Refusal = undefined;
    try testing.expectEqualStrings("/mnt/x", (try parse(av(&.{ "umount", "/mnt/x" }), &d)).umount.mountpoint);
    try testing.expectEqualStrings("sqmeta:///v", (try parse(av(&.{ "status", "sqmeta:///v" }), &d)).status.meta_uri);
    try testing.expect((try refusal(av(&.{"umount"}))) == .missing_positional);
    try testing.expect((try refusal(av(&.{ "umount", "/a", "/b" }))) == .extra_positional);
    try testing.expect((try refusal(av(&.{ "umount", "--lazy", "/a" }))) == .unknown_flag);
    try testing.expect((try refusal(av(&.{ "status", "/dev/v" }))) == .bad_uri);
    try testing.expect((try refusal(av(&.{"status"}))) == .missing_positional);
}

test "usage names every verb" {
    try testing.expect(std.mem.indexOf(u8, usage(null), "squeezefs format") != null);
    try testing.expect(std.mem.indexOf(u8, usage(.mount), "--supervise") != null);
    try testing.expect(std.mem.indexOf(u8, usage(.umount), "<mountpoint>") != null);
}
