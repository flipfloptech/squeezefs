//! The CLI grammar (charter §8): `format`, `mount`, `umount`, `status`,
//! `clients`, `df`, `bench`, `clone`, `tune`, `config`, `staging`, `claim
//! clear`, `--version`, `--help`, parsed into ONE typed `Verb`. Flags and
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
const transform = @import("../data/transform.zig");
const set_config = @import("../data/set_config.zig");
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
    /// `--block-size BYTES`: the data block, a power of two in
    /// [`transform.MIN_BLOCK_SIZE`, `transform.MAX_BLOCK_SIZE`]; the
    /// charter's 4 MiB when absent.
    block_size: u32 = transform.DEFAULT_BLOCK_SIZE,
    /// `--compress none|lz4|zstd` (charter §6.5).
    compression: transform.Compression = .none,
    /// `--encrypt none|aes-256-gcm|chacha20-poly1305`; requires
    /// `--encrypt-key`.
    encryption: transform.Encryption = .none,
    /// `--encrypt-key PATH|-`: the operator key file (stdin as `-`).
    encrypt_key: ?[]const u8 = null,
    /// `--disk-cache-paths a,b`: absolute staging/cache roots recorded in
    /// the set config (none = cache-less).
    disk_cache_paths: []const u8 = "",
};

pub const Mount = struct {
    meta_uri: []const u8,
    mountpoint: []const u8,
    /// The raw `-o` string (the mount verb parses it over the env TTLs).
    options: ?[]const u8 = null,
    daemon: bool = false,
    supervise: bool = false,
    log_file: ?[]const u8 = null,
    /// `--join host:port`: the live manager's listener when the durable
    /// `endpoint:` record is not to be trusted (a manager that moved its
    /// listener before its checkpoint landed).
    join: ?[]const u8 = null,
};

pub const Umount = struct {
    mountpoint: []const u8,
};

/// A `sqmeta://` URI or a mountpoint (resolved through the mount's
/// `.config`): the ONE positional every set-level verb takes
/// (`docs/porting/stats_cli.md` §2 — no `-g`, no env default, no alias).
pub const Target = []const u8;

pub const Status = struct {
    target: Target,
};

pub const Clients = struct {
    target: Target,
    json: bool = false,
};

pub const Df = struct {
    target: Target,
    json: bool = false,
};

pub const Clone = struct {
    target: Target,
    /// Absolute paths INSIDE the set (`/dir/file`).
    src: []const u8,
    dest: []const u8,
};

pub const Tune = struct {
    apply: bool = false,
};

pub const Config = union(enum) {
    show: struct { target: Target, json: bool = false },
    set_cache_paths: struct { target: Target, paths: []const u8, force: bool = false },
};

pub const Staging = union(enum) {
    list: struct { target: Target, json: bool = false },
    promote: struct { target: Target },
};

pub const ClaimClear = struct {
    meta_uri: []const u8,
};

/// `squeezefs appenders [--json] sqmeta://<path>`.
pub const Appenders = struct {
    meta_uri: []const u8,
    json: bool = false,
};

/// `squeezefs appender clear sqmeta://<path> <id>` (ids ≥ 1; 0 refuses at
/// the verb — a dead manager's region is the D0 successor's).
pub const AppenderClear = struct {
    meta_uri: []const u8,
    appender_id: u32,
};

/// `--take-slots k | s1,s2,…` (`add-meta`): a count or an explicit list.
pub const TakeSlots = union(enum) {
    count: u32,
    /// The raw comma list; the verb parses each member as a slot id.
    list: []const u8,
};

/// `volume {list, add-meta, remove-meta, repair-set, migrate-meta-slot}`
/// (charter §8; `docs/porting/dynamic_routing.md` §3, §4): every verb takes
/// the set's `sqmeta://` URI (`add-meta`/`remove-meta` name the device
/// too); the online engine's verb takes the slot and the target member.
pub const Volume = union(enum) {
    list: struct { meta_uri: []const u8 },
    add_meta: struct { meta_uri: []const u8, device: []const u8, take: TakeSlots },
    remove_meta: struct { meta_uri: []const u8, victim: []const u8 },
    repair_set: struct { meta_uri: []const u8 },
    migrate_meta_slot: struct { meta_uri: []const u8, slot: u16, target: u16 },
};

/// `bench` (stats_cli.md §3): the shape vocabulary `threads × files × size
/// @ block`; null = auto-derived from the mount's geometry; the bare
/// invocation (no phase flag) runs the saturation suite.
pub const Bench = struct {
    mountpoint: []const u8,
    write: bool = false,
    read: bool = false,
    stat: bool = false,
    del: bool = false,
    threads: ?u32 = null,
    files: ?u32 = null,
    size: ?u64 = null,
    block: ?u64 = null,
    rand: bool = false,
    direct: bool = false,
    /// Seconds; null = the default per access pattern; 0 = full coverage.
    time: ?u32 = null,
    iterations: u32 = 1,
    /// One iteration of every workload on a tiny shape (the gate).
    smoke: bool = false,
    /// A-B-B-A over the one lever the client controls (O_DIRECT vs
    /// buffered): A = `--direct` as given, B = its opposite.
    abba: bool = false,
    json: bool = false,

    pub fn anyPhase(self: Bench) bool {
        return self.write or self.read or self.stat or self.del;
    }
};

pub const Verb = union(enum) {
    version,
    help,
    format: Format,
    mount: Mount,
    umount: Umount,
    status: Status,
    clients: Clients,
    df: Df,
    bench: Bench,
    clone: Clone,
    tune: Tune,
    config: Config,
    staging: Staging,
    claim_clear: ClaimClear,
    appenders: Appenders,
    appender_clear: AppenderClear,
    volume: Volume,
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
    /// `--block-size` outside the power-of-two window.
    bad_block_size: u64,
    bad_compression: []const u8,
    bad_encryption: []const u8,
    /// `--encrypt` names an algorithm but no `--encrypt-key` was given.
    encrypt_without_key,
    /// `--encrypt-key` on a plaintext set.
    key_without_encrypt,
    bad_cache_path: []const u8,
    /// A set-level target must be a `sqmeta://` URI or an absolute mountpoint.
    bad_target: []const u8,
    /// A clone path must be absolute inside the set.
    bad_set_path: []const u8,
    /// `config`/`staging`/`claim` need a sub-verb.
    missing_subverb: []const u8,
    unknown_subverb: struct { verb: []const u8, word: []const u8 },
    /// `bench`: a size word that is not `<int>[k|m|g|t]`.
    bad_size: struct { flag: []const u8, word: []const u8 },
    bench_suite_with_shape: []const u8,
    /// `--take-slots` neither a count nor a comma list of slot ids.
    bad_take_slots: []const u8,

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
            .bad_block_size => |b| try w.print("format: --block-size {d} is not a power of two in [{d}, {d}]", .{ b, transform.MIN_BLOCK_SIZE, transform.MAX_BLOCK_SIZE }),
            .bad_compression => |c| try w.print("format: --compress '{s}' is not one of none|lz4|zstd", .{c}),
            .bad_encryption => |e| try w.print("format: --encrypt '{s}' is not one of none|aes-256-gcm|chacha20-poly1305", .{e}),
            .encrypt_without_key => try w.writeAll("format: --encrypt names an algorithm but no key was given — pass --encrypt-key <path|-> (≥ 32 bytes of key material, mode 0600)"),
            .key_without_encrypt => try w.writeAll("format: --encrypt-key was given for a plaintext set — pass --encrypt aes-256-gcm|chacha20-poly1305 or drop the key"),
            .bad_cache_path => |p| try w.print("cache path member '{s}' is not an absolute path outside the system roots (/, /home, /etc, /usr, /var, /boot, /root), or repeats", .{p}),
            .bad_target => |t| try w.print("'{s}' is neither a sqmeta://<absolute path>[,…] URI nor an absolute mountpoint", .{t}),
            .bad_set_path => |t| try w.print("clone: '{s}' is not an absolute path inside the set (`/dir/file`)", .{t}),
            .missing_subverb => |v| try w.print("{s}: missing the sub-verb", .{v}),
            .unknown_subverb => |u| try w.print("{s}: '{s}' is not a sub-verb", .{ u.verb, u.word }),
            .bad_size => |b| try w.print("bench: {s} '{s}' is not a size (<integer>[k|m|g|t], ≥ 1)", .{ b.flag, b.word }),
            .bench_suite_with_shape => |f| try w.print("bench: '{s}' needs a phase flag (-w/-r/--stat/--del) — the bare invocation runs the fixed saturation suite", .{f}),
            .bad_take_slots => |t| try w.print("volume add-meta: --take-slots '{s}' is neither a count (≥ 1) nor a comma list of slot ids (each < 65536)", .{t}),
        }
    }
};

pub const Error = error{Refused};

/// A verb's usage, one line per form.
pub fn usage(verb: ?std.meta.Tag(Verb)) []const u8 {
    const all =
        \\usage: squeezefs <verb> [flags] …
        \\  squeezefs format [--single-writer] [--meta-node-kib N] [--meta-journal-mb N] [--block-size BYTES] [--compress none|lz4|zstd] [--encrypt none|aes-256-gcm|chacha20-poly1305 --encrypt-key PATH|-] [--disk-cache-paths a,b] [--force] sqmeta://<path> [sqdata://<path>…]
        \\  squeezefs mount [-o opt[,opt…]] [--daemon --log-file PATH [--supervise]] [--join host:port] sqmeta://<path> <mountpoint>
        \\  squeezefs umount <mountpoint>
        \\  squeezefs status sqmeta://<path>|<mountpoint>
        \\  squeezefs clients [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs df [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs clone sqmeta://<path>|<mountpoint> /src/file /dest/file
        \\  squeezefs tune [--apply]
        \\  squeezefs config show [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs config set-cache-paths [--force] sqmeta://<path>|<mountpoint> <abs path>[,…]
        \\  squeezefs staging list [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs staging promote sqmeta://<path>
        \\  squeezefs claim clear sqmeta://<path>
        \\  squeezefs appenders [--json] sqmeta://<path>
        \\  squeezefs appender clear sqmeta://<path> <id>
        \\  squeezefs volume list sqmeta://<path>[,…]
        \\  squeezefs volume add-meta --take-slots K|s1,s2,… sqmeta://<path>[,…] <device>
        \\  squeezefs volume remove-meta sqmeta://<path>[,…] <victim device>
        \\  squeezefs volume repair-set sqmeta://<path>[,…]
        \\  squeezefs volume migrate-meta-slot sqmeta://<path>[,…] <slot> <target member index>
        \\  squeezefs bench [-w] [-r] [--stat] [--del] [-t N] [-n N] [-s SIZE] [-b SIZE] [--rand] [--direct] [--time S] [-i N] [--abba] [--json] [--smoke] <mountpoint>
        \\  squeezefs --version
        \\
    ;
    const v = verb orelse return all;
    return switch (v) {
        .format => "usage: squeezefs format [--single-writer] [--meta-node-kib 64|128|256|512|1024] [--meta-journal-mb N] [--block-size BYTES] [--compress none|lz4|zstd] [--encrypt none|aes-256-gcm|chacha20-poly1305 --encrypt-key PATH|-] [--disk-cache-paths a,b] [--force] sqmeta://<path> [sqdata://<path>…]\n",
        .mount => "usage: squeezefs mount [-o opt[,opt…]] [--daemon --log-file PATH [--supervise]] [--join host:port] sqmeta://<path> <mountpoint>\n",
        .umount => "usage: squeezefs umount <mountpoint>\n",
        .status => "usage: squeezefs status sqmeta://<path>|<mountpoint>\n",
        .clients => "usage: squeezefs clients [--json] sqmeta://<path>|<mountpoint>\n",
        .df => "usage: squeezefs df [--json] sqmeta://<path>|<mountpoint>\n",
        .clone => "usage: squeezefs clone sqmeta://<path>|<mountpoint> /src/file /dest/file\n",
        .tune => "usage: squeezefs tune [--apply]\n",
        .config => "usage: squeezefs config show [--json] sqmeta://<path>|<mountpoint>\n       squeezefs config set-cache-paths [--force] sqmeta://<path>|<mountpoint> <abs path>[,…]\n",
        .staging => "usage: squeezefs staging list [--json] sqmeta://<path>|<mountpoint>\n       squeezefs staging promote sqmeta://<path>\n",
        .claim_clear => "usage: squeezefs claim clear sqmeta://<path>\n",
        .appenders => "usage: squeezefs appenders [--json] sqmeta://<path>\n",
        .appender_clear => "usage: squeezefs appender clear sqmeta://<path> <id>\n",
        .volume => "usage: squeezefs volume list sqmeta://<path>[,…]\n       squeezefs volume add-meta --take-slots K|s1,s2,… sqmeta://<path>[,…] <device>\n       squeezefs volume remove-meta sqmeta://<path>[,…] <victim device>\n       squeezefs volume repair-set sqmeta://<path>[,…]\n       squeezefs volume migrate-meta-slot sqmeta://<path>[,…] <slot> <target member index>\n",
        .bench => "usage: squeezefs bench [-w] [-r] [--stat] [--del] [-t THREADS] [-n FILES] [-s SIZE] [-b BLOCK] [--rand] [--direct] [--time SECS] [-i ITERATIONS] [--abba] [--json] [--smoke] <mountpoint>\n",
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
    if (std.mem.eql(u8, verb, "clients")) return parseTargetVerb(.clients, words, diag);
    if (std.mem.eql(u8, verb, "df")) return parseTargetVerb(.df, words, diag);
    if (std.mem.eql(u8, verb, "clone")) return parseClone(words, diag);
    if (std.mem.eql(u8, verb, "tune")) return parseTune(words, diag);
    if (std.mem.eql(u8, verb, "config")) return parseConfig(words, diag);
    if (std.mem.eql(u8, verb, "staging")) return parseStaging(words, diag);
    if (std.mem.eql(u8, verb, "claim")) return parseClaim(words, diag);
    if (std.mem.eql(u8, verb, "appenders")) return parseAppenders(words, diag);
    if (std.mem.eql(u8, verb, "appender")) return parseAppenderClear(words, diag);
    if (std.mem.eql(u8, verb, "volume")) return parseVolume(words, diag);
    if (std.mem.eql(u8, verb, "bench")) return parseBench(words, diag);
    diag.* = .{ .unknown_verb = verb };
    return error.Refused;
}

/// A set-level target: a `sqmeta://` URI (validated by the ONE parser) or
/// an absolute mountpoint (resolved through `.config` by the verb).
pub fn checkTarget(word: []const u8, diag: *Refusal) Error!void {
    if (uri.schemeOf(word)) |scheme| {
        if (scheme != .meta) {
            diag.* = .{ .bad_target = word };
            return error.Refused;
        }
        return checkUri(.meta, word, diag);
    }
    if (word.len == 0 or word[0] != '/') {
        diag.* = .{ .bad_target = word };
        return error.Refused;
    }
}

/// Is the target a mountpoint (vs a URI)?
pub fn targetIsMountpoint(word: []const u8) bool {
    return uri.schemeOf(word) == null;
}

fn exactlyOne(c: *Cursor, what: []const u8) Error![]const u8 {
    const pos = c.pos();
    if (pos.len < 1) {
        c.diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = what } };
        return error.Refused;
    }
    if (pos.len > 1) {
        c.diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[1] } };
        return error.Refused;
    }
    return pos[0];
}

const target_what = "the sqmeta:// volume or the mountpoint";

/// `clients` / `df`: `[--json] TARGET`.
fn parseTargetVerb(comptime tag: std.meta.Tag(Verb), words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = @tagName(tag), .words = words, .diag = diag };
    var json = false;
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
    }
    const target = try exactlyOne(&c, target_what);
    try checkTarget(target, diag);
    return switch (tag) {
        .clients => .{ .clients = .{ .target = target, .json = json } },
        .df => .{ .df = .{ .target = target, .json = json } },
        else => comptime unreachable,
    };
}

fn checkSetPath(word: []const u8, diag: *Refusal) Error!void {
    if (word.len < 2 or word[0] != '/' or word[word.len - 1] == '/') {
        diag.* = .{ .bad_set_path = word };
        return error.Refused;
    }
}

fn parseClone(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "clone", .words = words, .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const pos = c.pos();
    if (pos.len < 1) {
        diag.* = .{ .missing_positional = .{ .verb = "clone", .what = target_what } };
        return error.Refused;
    }
    try checkTarget(pos[0], diag);
    if (pos.len < 2) {
        diag.* = .{ .missing_positional = .{ .verb = "clone", .what = "the source path" } };
        return error.Refused;
    }
    if (pos.len < 3) {
        diag.* = .{ .missing_positional = .{ .verb = "clone", .what = "the destination path" } };
        return error.Refused;
    }
    if (pos.len > 3) {
        diag.* = .{ .extra_positional = .{ .verb = "clone", .word = pos[3] } };
        return error.Refused;
    }
    try checkSetPath(pos[1], diag);
    try checkSetPath(pos[2], diag);
    return .{ .clone = .{ .target = pos[0], .src = pos[1], .dest = pos[2] } };
}

fn parseTune(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "tune", .words = words, .diag = diag };
    var out: Tune = .{};
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--apply")) out.apply = true else return c.unknown(f);
    }
    if (c.pos().len != 0) {
        diag.* = .{ .extra_positional = .{ .verb = "tune", .word = c.pos()[0] } };
        return error.Refused;
    }
    return .{ .tune = out };
}

fn subverb(verb: []const u8, words: []const []const u8, diag: *Refusal) Error![]const u8 {
    if (words.len == 0) {
        diag.* = .{ .missing_subverb = verb };
        return error.Refused;
    }
    return words[0];
}

fn parseConfig(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("config", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "show")) {
        var c: Cursor = .{ .verb = "config show", .words = rest, .diag = diag };
        var json = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
        }
        const target = try exactlyOne(&c, target_what);
        try checkTarget(target, diag);
        return .{ .config = .{ .show = .{ .target = target, .json = json } } };
    }
    if (std.mem.eql(u8, sub, "set-cache-paths")) {
        var c: Cursor = .{ .verb = "config set-cache-paths", .words = rest, .diag = diag };
        var force = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--force")) force = true else return c.unknown(f);
        }
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = target_what } };
            return error.Refused;
        }
        try checkTarget(pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the cache paths (comma-separated absolute paths)" } };
            return error.Refused;
        }
        if (pos.len > 2) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
            return error.Refused;
        }
        try checkCachePaths(pos[1], diag);
        return .{ .config = .{ .set_cache_paths = .{ .target = pos[0], .paths = pos[1], .force = force } } };
    }
    diag.* = .{ .unknown_subverb = .{ .verb = "config", .word = sub } };
    return error.Refused;
}

fn parseStaging(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("staging", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "list")) {
        var c: Cursor = .{ .verb = "staging list", .words = rest, .diag = diag };
        var json = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
        }
        const target = try exactlyOne(&c, target_what);
        try checkTarget(target, diag);
        return .{ .staging = .{ .list = .{ .target = target, .json = json } } };
    }
    if (std.mem.eql(u8, sub, "promote")) {
        var c: Cursor = .{ .verb = "staging promote", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const target = try exactlyOne(&c, "the sqmeta:// volume");
        try checkUri(.meta, target, diag);
        return .{ .staging = .{ .promote = .{ .target = target } } };
    }
    diag.* = .{ .unknown_subverb = .{ .verb = "staging", .word = sub } };
    return error.Refused;
}

fn parseClaim(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("claim", words, diag);
    if (!std.mem.eql(u8, sub, "clear")) {
        diag.* = .{ .unknown_subverb = .{ .verb = "claim", .word = sub } };
        return error.Refused;
    }
    var c: Cursor = .{ .verb = "claim clear", .words = words[1..], .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const target = try exactlyOne(&c, "the sqmeta:// volume");
    try checkUri(.meta, target, diag);
    return .{ .claim_clear = .{ .meta_uri = target } };
}

fn parseAppenders(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "appenders", .words = words, .diag = diag };
    var json = false;
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--json")) {
            json = true;
        } else return c.unknown(f);
    }
    const target = try exactlyOne(&c, "the sqmeta:// volume");
    try checkUri(.meta, target, diag);
    return .{ .appenders = .{ .meta_uri = target, .json = json } };
}

fn parseAppenderClear(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("appender", words, diag);
    if (!std.mem.eql(u8, sub, "clear")) {
        diag.* = .{ .unknown_subverb = .{ .verb = "appender", .word = sub } };
        return error.Refused;
    }
    var c: Cursor = .{ .verb = "appender clear", .words = words[1..], .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const pos = c.pos();
    if (pos.len < 2) {
        diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = if (pos.len == 0) "the sqmeta:// volume" else "the appender id" } };
        return error.Refused;
    }
    if (pos.len > 2) {
        diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
        return error.Refused;
    }
    try checkUri(.meta, pos[0], diag);
    // The id through the ONE integer parser (the verb's word as its name).
    const id: u32 = switch (knobs.parseIntIn(u32, "appender id", pos[1], 0, std.math.maxInt(u32))) {
        .value => |v| v,
        .absent => {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the appender id" } };
            return error.Refused;
        },
        .invalid => |d| {
            diag.* = .{ .bad_int = d };
            return error.Refused;
        },
    };
    return .{ .appender_clear = .{ .meta_uri = pos[0], .appender_id = id } };
}

fn parseVolume(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("volume", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "list") or std.mem.eql(u8, sub, "repair-set")) {
        const is_list = std.mem.eql(u8, sub, "list");
        var c: Cursor = .{ .verb = if (is_list) "volume list" else "volume repair-set", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const target = try exactlyOne(&c, "the sqmeta:// set");
        try checkUri(.meta, target, diag);
        return .{ .volume = if (is_list) .{ .list = .{ .meta_uri = target } } else .{ .repair_set = .{ .meta_uri = target } } };
    }
    if (std.mem.eql(u8, sub, "add-meta")) {
        var c: Cursor = .{ .verb = "volume add-meta", .words = rest, .diag = diag };
        var take: ?TakeSlots = null;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--take-slots")) {
                const raw = try c.value(f);
                take = parseTakeSlots(raw) orelse {
                    diag.* = .{ .bad_take_slots = raw };
                    return error.Refused;
                };
            } else return c.unknown(f);
        }
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// set" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the device to add (an absolute path)" } };
            return error.Refused;
        }
        if (pos.len > 2) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
            return error.Refused;
        }
        if (pos[1].len == 0 or pos[1][0] != '/') {
            diag.* = .{ .bad_target = pos[1] };
            return error.Refused;
        }
        return .{ .volume = .{ .add_meta = .{ .meta_uri = pos[0], .device = pos[1], .take = take orelse {
            diag.* = .{ .missing_value = .{ .verb = c.verb, .flag = "--take-slots" } };
            return error.Refused;
        } } } };
    }
    if (std.mem.eql(u8, sub, "remove-meta")) {
        var c: Cursor = .{ .verb = "volume remove-meta", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// set" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the victim device (an absolute path)" } };
            return error.Refused;
        }
        if (pos.len > 2) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
            return error.Refused;
        }
        if (pos[1].len == 0 or pos[1][0] != '/') {
            diag.* = .{ .bad_target = pos[1] };
            return error.Refused;
        }
        return .{ .volume = .{ .remove_meta = .{ .meta_uri = pos[0], .victim = pos[1] } } };
    }
    if (std.mem.eql(u8, sub, "migrate-meta-slot")) {
        var c: Cursor = .{ .verb = "volume migrate-meta-slot", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// set" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the slot (0..65535)" } };
            return error.Refused;
        }
        if (pos.len < 3) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the target member index" } };
            return error.Refused;
        }
        if (pos.len > 3) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[3] } };
            return error.Refused;
        }
        const slot = try parseIntWord(diag, "slot", pos[1], 0, std.math.maxInt(u16));
        const target = try parseIntWord(diag, "target member index", pos[2], 0, uri.MAX_MEMBERS - 1);
        return .{ .volume = .{ .migrate_meta_slot = .{ .meta_uri = pos[0], .slot = @intCast(slot), .target = @intCast(target) } } };
    }
    diag.* = .{ .unknown_subverb = .{ .verb = "volume", .word = sub } };
    return error.Refused;
}

/// A positional integer through the ONE parser.
fn parseIntWord(diag: *Refusal, what: []const u8, word: []const u8, lo: u32, hi: u32) Error!u32 {
    return switch (knobs.parseIntIn(u32, what, word, lo, hi)) {
        .value => |v| v,
        .absent => blk: {
            diag.* = .{ .missing_positional = .{ .verb = "volume", .what = what } };
            break :blk error.Refused;
        },
        .invalid => |d| blk: {
            diag.* = .{ .bad_int = d };
            break :blk error.Refused;
        },
    };
}

/// `K` (≥ 1) or `s1,s2,…` (each a slot id < 65536; a list of one is a list).
pub fn parseTakeSlots(raw: []const u8) ?TakeSlots {
    if (raw.len == 0) return null;
    if (std.mem.indexOfScalar(u8, raw, ',') == null) {
        if (std.fmt.parseInt(u32, raw, 10)) |k| return if (k >= 1) .{ .count = k } else null else |_| return null;
    }
    var it = std.mem.splitScalar(u8, raw, ',');
    while (it.next()) |m| _ = std.fmt.parseInt(u16, m, 10) catch return null;
    return .{ .list = raw };
}

/// `<integer>[k|m|g|t]` (powers of 1024), ≥ 1.
pub fn parseSize(word: []const u8) ?u64 {
    if (word.len == 0) return null;
    var digits = word;
    var shift: u6 = 0;
    switch (std.ascii.toLower(word[word.len - 1])) {
        'k' => shift = 10,
        'm' => shift = 20,
        'g' => shift = 30,
        't' => shift = 40,
        '0'...'9' => {},
        else => return null,
    }
    if (shift != 0) digits = word[0 .. word.len - 1];
    const n = std.fmt.parseInt(u64, digits, 10) catch return null;
    if (n == 0) return null;
    if (shift != 0 and n > (@as(u64, std.math.maxInt(u64)) >> shift)) return null;
    return n << shift;
}

fn parseSizeFlag(c: *Cursor, f: Flag) Error!u64 {
    const raw = try c.value(f);
    return parseSize(raw) orelse {
        c.diag.* = .{ .bad_size = .{ .flag = f.name, .word = raw } };
        return error.Refused;
    };
}

fn parseBench(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "bench", .words = words, .diag = diag };
    var out: Bench = .{ .mountpoint = undefined };
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "-w") or std.mem.eql(u8, f.name, "--write")) {
            out.write = true;
        } else if (std.mem.eql(u8, f.name, "-r") or std.mem.eql(u8, f.name, "--read")) {
            out.read = true;
        } else if (std.mem.eql(u8, f.name, "--stat")) {
            out.stat = true;
        } else if (std.mem.eql(u8, f.name, "--del")) {
            out.del = true;
        } else if (std.mem.eql(u8, f.name, "-t") or std.mem.eql(u8, f.name, "--threads")) {
            out.threads = try parseIntFlag(&c, f, u32, 1, 4096);
        } else if (std.mem.eql(u8, f.name, "-n") or std.mem.eql(u8, f.name, "--files")) {
            out.files = try parseIntFlag(&c, f, u32, 1, 1 << 20);
        } else if (std.mem.eql(u8, f.name, "-s") or std.mem.eql(u8, f.name, "--size")) {
            out.size = try parseSizeFlag(&c, f);
        } else if (std.mem.eql(u8, f.name, "-b") or std.mem.eql(u8, f.name, "--block")) {
            out.block = try parseSizeFlag(&c, f);
        } else if (std.mem.eql(u8, f.name, "--rand")) {
            out.rand = true;
        } else if (std.mem.eql(u8, f.name, "--direct")) {
            out.direct = true;
        } else if (std.mem.eql(u8, f.name, "--time")) {
            out.time = try parseIntFlag(&c, f, u32, 0, 86_400);
        } else if (std.mem.eql(u8, f.name, "-i") or std.mem.eql(u8, f.name, "--iterations")) {
            out.iterations = try parseIntFlag(&c, f, u32, 1, 1 << 20);
        } else if (std.mem.eql(u8, f.name, "--smoke")) {
            out.smoke = true;
        } else if (std.mem.eql(u8, f.name, "--abba")) {
            out.abba = true;
        } else if (std.mem.eql(u8, f.name, "--json")) {
            out.json = true;
        } else return c.unknown(f);
    }
    out.mountpoint = try exactlyOne(&c, "the mountpoint");
    if (out.mountpoint.len == 0 or out.mountpoint[0] != '/') {
        diag.* = .{ .bad_target = out.mountpoint };
        return error.Refused;
    }
    // The bare invocation runs the fixed suite: a block or an access
    // pattern without a phase would silently reshape it.
    if (!out.anyPhase() and !out.smoke) {
        if (out.block != null) {
            diag.* = .{ .bench_suite_with_shape = "-b" };
            return error.Refused;
        }
        if (out.rand) {
            diag.* = .{ .bench_suite_with_shape = "--rand" };
            return error.Refused;
        }
    }
    return .{ .bench = out };
}

/// The `--disk-cache-paths` / `set-cache-paths` member law: absolute,
/// outside the system roots, no repeats (`set_config.cachePathAdmissible`).
fn checkCachePaths(raw: []const u8, diag: *Refusal) Error!void {
    var seen: [MAX_CACHE_PATHS][]const u8 = undefined;
    var n_seen: usize = 0;
    var it = cachePaths(raw);
    while (it.next()) |path| {
        if (!set_config.cachePathAdmissible(path) or n_seen == seen.len) {
            diag.* = .{ .bad_cache_path = path };
            return error.Refused;
        }
        for (seen[0..n_seen]) |q| if (std.mem.eql(u8, q, path)) {
            diag.* = .{ .bad_cache_path = path };
            return error.Refused;
        };
        seen[n_seen] = path;
        n_seen += 1;
    }
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
        } else if (std.mem.eql(u8, f.name, "--block-size")) {
            const b = try parseIntFlag(&c, f, u64, transform.MIN_BLOCK_SIZE, transform.MAX_BLOCK_SIZE);
            if (!transform.blockSizeValid(b)) {
                diag.* = .{ .bad_block_size = b };
                return error.Refused;
            }
            out.block_size = @intCast(b);
        } else if (std.mem.eql(u8, f.name, "--compress")) {
            const v = try c.value(f);
            out.compression = transform.Compression.parse(v) orelse {
                diag.* = .{ .bad_compression = v };
                return error.Refused;
            };
        } else if (std.mem.eql(u8, f.name, "--encrypt")) {
            const v = try c.value(f);
            out.encryption = transform.Encryption.parse(v) orelse {
                diag.* = .{ .bad_encryption = v };
                return error.Refused;
            };
        } else if (std.mem.eql(u8, f.name, "--encrypt-key")) {
            out.encrypt_key = try c.value(f);
        } else if (std.mem.eql(u8, f.name, "--disk-cache-paths")) {
            out.disk_cache_paths = try c.value(f);
        } else return c.unknown(f);
    }
    if (out.encryption != .none and out.encrypt_key == null) {
        diag.* = .encrypt_without_key;
        return error.Refused;
    }
    if (out.encryption == .none and out.encrypt_key != null) {
        diag.* = .key_without_encrypt;
        return error.Refused;
    }
    try checkCachePaths(out.disk_cache_paths, diag);
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

/// `--disk-cache-paths a,b` as an iterator over its members (empty = none).
pub fn cachePaths(raw: []const u8) CachePathIterator {
    return .{ .inner = std.mem.splitScalar(u8, raw, ','), .empty = raw.len == 0 };
}

pub const CachePathIterator = struct {
    inner: std.mem.SplitIterator(u8, .scalar),
    empty: bool,

    pub fn next(self: *CachePathIterator) ?[]const u8 {
        if (self.empty) return null;
        return self.inner.next();
    }
};

/// `set_config.MAX_CACHE_PATHS`: the grammar's own bound on one argv word.
pub const MAX_CACHE_PATHS: usize = set_config.MAX_CACHE_PATHS;

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
        } else if (std.mem.eql(u8, f.name, "--join")) {
            out.join = try c.value(f);
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
    const target = try exactlyOne(&c, target_what);
    try checkTarget(target, diag);
    return .{ .status = .{ .target = target } };
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

test "format: the data-plane flags — block size (power of two in the window), compress, encrypt + key (both or neither), cache paths (absolute, no system root, no repeat)" {
    var d: Refusal = undefined;
    var buf: [512]u8 = undefined;
    const v = try parse(av(&.{ "format", "--block-size", "1048576", "--compress=zstd", "--encrypt", "chacha20-poly1305", "--encrypt-key", "/etc/squeezefs/keys/x.key", "--disk-cache-paths", "/build/c1,/build/c2", "sqmeta:///m", "sqdata:///d" }), &d);
    try testing.expectEqual(@as(u32, 1 << 20), v.format.block_size);
    try testing.expectEqual(transform.Compression.zstd, v.format.compression);
    try testing.expectEqual(transform.Encryption.chacha20poly1305, v.format.encryption);
    try testing.expectEqualStrings("/etc/squeezefs/keys/x.key", v.format.encrypt_key.?);
    var it = cachePaths(v.format.disk_cache_paths);
    try testing.expectEqualStrings("/build/c1", it.next().?);
    try testing.expectEqualStrings("/build/c2", it.next().?);
    try testing.expect(it.next() == null);
    const plain = try parse(av(&.{ "format", "sqmeta:///m" }), &d);
    try testing.expectEqual(transform.DEFAULT_BLOCK_SIZE, plain.format.block_size);
    try testing.expect(plain.format.compression == .none and plain.format.encryption == .none and plain.format.encrypt_key == null);
    var none = cachePaths(plain.format.disk_cache_paths);
    try testing.expect(none.next() == null);
    const stdin_key = try parse(av(&.{ "format", "--encrypt", "aes-256-gcm", "--encrypt-key", "-", "sqmeta:///m" }), &d);
    try testing.expectEqualStrings("-", stdin_key.format.encrypt_key.?);
    var r = try refusal(av(&.{ "format", "--block-size", "3145728", "sqmeta:///m" }));
    try testing.expect(r == .bad_block_size);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "power of two") != null);
    r = try refusal(av(&.{ "format", "--block-size", "2048", "sqmeta:///m" }));
    try testing.expect(r == .bad_int);
    r = try refusal(av(&.{ "format", "--compress", "gzip", "sqmeta:///m" }));
    try testing.expect(r == .bad_compression);
    r = try refusal(av(&.{ "format", "--encrypt", "aes256gcm", "sqmeta:///m" }));
    try testing.expect(r == .bad_encryption);
    r = try refusal(av(&.{ "format", "--encrypt", "aes-256-gcm", "sqmeta:///m" }));
    try testing.expect(r == .encrypt_without_key);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "--encrypt-key") != null);
    r = try refusal(av(&.{ "format", "--encrypt-key", "/k", "sqmeta:///m" }));
    try testing.expect(r == .key_without_encrypt);
    r = try refusal(av(&.{ "format", "--disk-cache-paths", "/build/c,/home", "sqmeta:///m" }));
    try testing.expect(r == .bad_cache_path);
    try testing.expectEqualStrings("/home", r.bad_cache_path);
    r = try refusal(av(&.{ "format", "--disk-cache-paths", "/build/c,/build/c", "sqmeta:///m" }));
    try testing.expect(r == .bad_cache_path);
    r = try refusal(av(&.{ "format", "--disk-cache-paths", "rel", "sqmeta:///m" }));
    try testing.expect(r == .bad_cache_path);
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
    try testing.expectEqualStrings("sqmeta:///v", (try parse(av(&.{ "status", "sqmeta:///v" }), &d)).status.target);
    try testing.expectEqualStrings("/mnt/sqz", (try parse(av(&.{ "status", "/mnt/sqz" }), &d)).status.target);
    try testing.expect((try refusal(av(&.{ "status", "sqdata:///d" }))) == .bad_target);
    try testing.expect((try refusal(av(&.{ "status", "relative" }))) == .bad_target);
    try testing.expect((try refusal(av(&.{"umount"}))) == .missing_positional);
    try testing.expect((try refusal(av(&.{ "umount", "/a", "/b" }))) == .extra_positional);
    try testing.expect((try refusal(av(&.{ "umount", "--lazy", "/a" }))) == .unknown_flag);
    try testing.expect((try refusal(av(&.{"status"}))) == .missing_positional);
}

test "rung-5 verbs: clients/df/config show/staging list take [--json] TARGET (URI or mountpoint); clone TARGET SRC DEST with absolute set paths; tune [--apply]; set-cache-paths validates the member law; staging promote and claim clear take a URI only; sub-verbs refuse by name" {
    var d: Refusal = undefined;
    const cl = try parse(av(&.{ "clients", "--json", "/mnt/sqz" }), &d);
    try testing.expect(cl.clients.json and std.mem.eql(u8, cl.clients.target, "/mnt/sqz"));
    const df = try parse(av(&.{ "df", "sqmeta:///dev/m" }), &d);
    try testing.expect(!df.df.json and targetIsMountpoint("/mnt") and !targetIsMountpoint(df.df.target));
    const cn = try parse(av(&.{ "clone", "sqmeta:///dev/m", "/a/b", "/a/c" }), &d);
    try testing.expectEqualStrings("/a/b", cn.clone.src);
    try testing.expectEqualStrings("/a/c", cn.clone.dest);
    try testing.expect((try refusal(av(&.{ "clone", "sqmeta:///dev/m", "a/b", "/a/c" }))) == .bad_set_path);
    try testing.expect((try refusal(av(&.{ "clone", "sqmeta:///dev/m", "/a/b", "/a/c/" }))) == .bad_set_path);
    try testing.expect((try refusal(av(&.{ "clone", "sqmeta:///dev/m", "/a/b" }))) == .missing_positional);
    try testing.expect((try refusal(av(&.{ "clone", "sqmeta:///dev/m", "/a/b", "/a/c", "/x" }))) == .extra_positional);
    try testing.expect((try parse(av(&.{ "tune", "--apply" }), &d)).tune.apply);
    try testing.expect(!(try parse(av(&.{"tune"}), &d)).tune.apply);
    try testing.expect((try refusal(av(&.{ "tune", "x" }))) == .extra_positional);
    const cs = try parse(av(&.{ "config", "show", "--json", "sqmeta:///dev/m" }), &d);
    try testing.expect(cs.config.show.json);
    const cp = try parse(av(&.{ "config", "set-cache-paths", "--force", "/mnt/sqz", "/build/c1,/build/c2" }), &d);
    try testing.expect(cp.config.set_cache_paths.force);
    try testing.expectEqualStrings("/build/c1,/build/c2", cp.config.set_cache_paths.paths);
    try testing.expect((try refusal(av(&.{ "config", "set-cache-paths", "/mnt/sqz", "/home" }))) == .bad_cache_path);
    try testing.expect((try refusal(av(&.{ "config", "set-cache-paths", "/mnt/sqz" }))) == .missing_positional);
    try testing.expect((try refusal(av(&.{"config"}))) == .missing_subverb);
    try testing.expect((try refusal(av(&.{ "config", "list" }))) == .unknown_subverb);
    const sl = try parse(av(&.{ "staging", "list", "/mnt/sqz" }), &d);
    try testing.expectEqualStrings("/mnt/sqz", sl.staging.list.target);
    const sp = try parse(av(&.{ "staging", "promote", "sqmeta:///dev/m" }), &d);
    try testing.expectEqualStrings("sqmeta:///dev/m", sp.staging.promote.target);
    try testing.expect((try refusal(av(&.{ "staging", "promote", "/mnt/sqz" }))) == .bad_uri);
    try testing.expect((try refusal(av(&.{ "staging", "drain", "sqmeta:///dev/m" }))) == .unknown_subverb);
    const cc = try parse(av(&.{ "claim", "clear", "sqmeta:///dev/m,/dev/n" }), &d);
    try testing.expectEqualStrings("sqmeta:///dev/m,/dev/n", cc.claim_clear.meta_uri);
    try testing.expect((try refusal(av(&.{ "claim", "clear", "/mnt" }))) == .bad_uri);
    try testing.expect((try refusal(av(&.{ "claim", "show", "sqmeta:///dev/m" }))) == .unknown_subverb);
}

test "bench: every flag; sizes accept k/m/g/t and refuse garbage and 0; the bare invocation refuses -b and --rand; iterations ≥ 1; the mountpoint is absolute" {
    var d: Refusal = undefined;
    const b = try parse(av(&.{ "bench", "-w", "-r", "--stat", "--del", "-t", "4", "-n", "2", "-s", "64m", "-b", "4k", "--rand", "--direct", "--time", "30", "-i", "3", "--json", "/mnt/sqz" }), &d);
    try testing.expect(b.bench.write and b.bench.read and b.bench.stat and b.bench.del and b.bench.rand and b.bench.direct and b.bench.json);
    try testing.expectEqual(@as(?u32, 4), b.bench.threads);
    try testing.expectEqual(@as(?u32, 2), b.bench.files);
    try testing.expectEqual(@as(?u64, 64 << 20), b.bench.size);
    try testing.expectEqual(@as(?u64, 4096), b.bench.block);
    try testing.expectEqual(@as(?u32, 30), b.bench.time);
    try testing.expectEqual(@as(u32, 3), b.bench.iterations);
    const bare = try parse(av(&.{ "bench", "/mnt/sqz" }), &d);
    try testing.expect(!bare.bench.anyPhase() and bare.bench.iterations == 1 and bare.bench.threads == null);
    try testing.expect((try parse(av(&.{ "bench", "--smoke", "/mnt/sqz" }), &d)).bench.smoke);
    try testing.expect((try parse(av(&.{ "bench", "-r", "--abba", "/mnt/sqz" }), &d)).bench.abba);
    try testing.expectEqual(@as(?u64, 1), parseSize("1"));
    try testing.expectEqual(@as(?u64, 1 << 10), parseSize("1k"));
    try testing.expectEqual(@as(?u64, 3 << 20), parseSize("3M"));
    try testing.expectEqual(@as(?u64, 2 << 30), parseSize("2g"));
    try testing.expectEqual(@as(?u64, 1 << 40), parseSize("1T"));
    try testing.expect(parseSize("") == null and parseSize("0") == null and parseSize("4x") == null and parseSize("k") == null and parseSize("-1") == null and parseSize("99999999999999999999g") == null);
    try testing.expect((try refusal(av(&.{ "bench", "-s", "junk", "/mnt/sqz" }))) == .bad_size);
    try testing.expect((try refusal(av(&.{ "bench", "-b", "4k", "/mnt/sqz" }))) == .bench_suite_with_shape);
    try testing.expect((try refusal(av(&.{ "bench", "--rand", "/mnt/sqz" }))) == .bench_suite_with_shape);
    try testing.expect((try refusal(av(&.{ "bench", "-i", "0", "/mnt/sqz" }))) == .bad_int);
    try testing.expect((try refusal(av(&.{ "bench", "mnt" }))) == .bad_target);
    try testing.expect((try refusal(av(&.{"bench"}))) == .missing_positional);
}

test "appenders [--json] TARGET and appender clear TARGET ID: the URI checked, the id through the one integer parser, sub-verbs refused by name" {
    var d: Refusal = undefined;
    const a = try parse(av(&.{ "appenders", "--json", "sqmeta:///dev/m" }), &d);
    try testing.expect(a.appenders.json);
    try testing.expectEqualStrings("sqmeta:///dev/m", a.appenders.meta_uri);
    try testing.expect((try refusal(av(&.{ "appenders", "/mnt" }))) == .bad_uri);
    const c = try parse(av(&.{ "appender", "clear", "sqmeta:///dev/m", "3" }), &d);
    try testing.expectEqual(@as(u32, 3), c.appender_clear.appender_id);
    try testing.expect((try refusal(av(&.{ "appender", "clear", "sqmeta:///dev/m" }))) == .missing_positional);
    try testing.expect((try refusal(av(&.{ "appender", "clear", "sqmeta:///dev/m", "x" }))) == .bad_int);
    try testing.expect((try refusal(av(&.{ "appender", "clear", "sqmeta:///dev/m", "1", "2" }))) == .extra_positional);
    try testing.expect((try refusal(av(&.{ "appender", "show", "sqmeta:///dev/m", "1" }))) == .unknown_subverb);
}

test "usage names every verb" {
    try testing.expect(std.mem.indexOf(u8, usage(null), "squeezefs format") != null);
    inline for (@typeInfo(std.meta.Tag(Verb)).@"enum".field_names) |name| {
        if (!std.mem.eql(u8, name, "version") and !std.mem.eql(u8, name, "help")) {
            const word = comptime if (std.mem.eql(u8, name, "claim_clear")) "squeezefs claim clear" else if (std.mem.eql(u8, name, "appender_clear")) "squeezefs appender clear" else "squeezefs " ++ name;
            try testing.expect(std.mem.indexOf(u8, usage(null), word) != null);
        }
    }
    try testing.expect(std.mem.indexOf(u8, usage(.mount), "--supervise") != null);
    try testing.expect(std.mem.indexOf(u8, usage(.umount), "<mountpoint>") != null);
}
