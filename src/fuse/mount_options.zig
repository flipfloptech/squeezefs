//! Mount options: the canonical `MountOptions` value, the ONE `-o` option
//! parser, the kernel TTL classes, and the two mount strings (the root
//! `mount(2)` data and the `fusermount3 -o` string) with the `MS_*` flags
//! (`docs/porting/fuse_abi_session.md` §10; charter §4 "per-class kernel TTL
//! mount options (attr/entry/dir-entry/negative, default 1 s)").
//!
//! One parser (Q11): every option is recognised by name; an integer option
//! goes through `knobs.parseIntIn` (the ONE integer parser, the row-style
//! inclusive range, refuse never clamp); a TTL option is canonical decimal
//! seconds with an optional fraction of at most nine digits (`1`, `0.5`,
//! `2.250`) — a unit suffix (`5s`), a sign, an exponent or any other junk
//! REFUSES THE MOUNT naming the option (charter §1.6: never silently
//! ignored; the reference parsed `f64` and dropped junk on the floor). An
//! option this daemon does not know refuses too: the kernel would have
//! refused it anyway ("Unknown parameter"), later and less clearly.
//!
//! Daemon-level options (`max_background=`, `congestion_threshold=`, the
//! four `*_timeout=`, `direct_device_true`, `stripe_dirs`, `writeback`) never
//! reach the kernel string. `fsname` (the `source` column) and `subtype`
//! (`fuse.<subtype>`) are INDEPENDENT: xfstests passes the device path as
//! `fsname`.
//!
//! TTL precedence: 1 s default → `SQUEEZEFS_FUSE_{ATTR,ENTRY,DIR_ENTRY,
//! NEGATIVE}_TTL_MS` → `-o *_timeout=`. Directories take the `dir_entry`
//! class for their dentry (a dir dentry's invalidation covers a subtree),
//! everything else `entry`; GETATTR/SETATTR and the attr half of every entry
//! reply take `attr`; a LOOKUP miss replies a negative entry for `negative`
//! (0 = the bare ENOENT).

const std = @import("std");
const linux = std.os.linux;
const knobs = @import("../core/knobs.zig");

const ns_per_s: u64 = std.time.ns_per_s;
const ns_per_ms: u64 = std.time.ns_per_ms;

/// The shipped default of every class (fuse_client.rs:345–353).
pub const DEFAULT_TTL_NS: u64 = 1 * ns_per_s;
/// The fraction grammar admits at most nanosecond resolution: the wire's
/// `*_valid_nsec` is a nanosecond field.
pub const MAX_FRACTION_DIGITS: usize = 9;
/// `rootmode` default: a directory (the kernel parses it octal).
pub const DEFAULT_ROOTMODE: u32 = 0o40000;
/// The `source` column when no `fsname=` was given.
pub const DEFAULT_FSNAME = "squeezefs";
/// The filesystem type suffix: `fuse.squeezefs`.
pub const SUBTYPE = "squeezefs";

pub const TtlClass = enum { attr, entry, dir_entry, negative };

pub const Ttls = struct {
    attr_ns: u64 = DEFAULT_TTL_NS,
    entry_ns: u64 = DEFAULT_TTL_NS,
    dir_entry_ns: u64 = DEFAULT_TTL_NS,
    negative_ns: u64 = DEFAULT_TTL_NS,

    /// The env layer: each class's `_TTL_MS` row through the registry
    /// reader (the row's `1000` is THE default).
    pub fn fromEnv(env: *const std.process.Environ.Map) Ttls {
        return .{
            .attr_ns = knobs.intKnob("SQUEEZEFS_FUSE_ATTR_TTL_MS", u64, env) * ns_per_ms,
            .entry_ns = knobs.intKnob("SQUEEZEFS_FUSE_ENTRY_TTL_MS", u64, env) * ns_per_ms,
            .dir_entry_ns = knobs.intKnob("SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", u64, env) * ns_per_ms,
            .negative_ns = knobs.intKnob("SQUEEZEFS_FUSE_NEGATIVE_TTL_MS", u64, env) * ns_per_ms,
        };
    }

    pub fn get(self: Ttls, class: TtlClass) u64 {
        return switch (class) {
            .attr => self.attr_ns,
            .entry => self.entry_ns,
            .dir_entry => self.dir_entry_ns,
            .negative => self.negative_ns,
        };
    }

    pub fn set(self: *Ttls, class: TtlClass, ns: u64) void {
        switch (class) {
            .attr => self.attr_ns = ns,
            .entry => self.entry_ns = ns,
            .dir_entry => self.dir_entry_ns = ns,
            .negative => self.negative_ns = ns,
        }
    }

    /// The dentry class of an entry reply: `dir_entry` for a directory.
    pub fn entryTtlFor(self: Ttls, is_dir: bool) u64 {
        return if (is_dir) self.dir_entry_ns else self.entry_ns;
    }

    /// Whether a LOOKUP miss is a cacheable negative entry (else bare ENOENT).
    pub fn negativeEnabled(self: Ttls) bool {
        return self.negative_ns != 0;
    }
};

/// The canonical options value. Kernel-facing fields map one-to-one onto
/// the mount strings; daemon-level fields never leave the process.
pub const MountOptions = struct {
    // ---- kernel: the data string ----
    /// `fsname=` (the mount table's source column); null ⇒ `squeezefs`.
    fs_name: ?[]const u8 = null,
    /// `subtype=`: always `squeezefs` (type `fuse.squeezefs`) unless cleared.
    subtype: ?[]const u8 = SUBTYPE,
    allow_other: bool = false,
    /// The daemon's posture: the kernel enforces permissions (and the
    /// `.stats` inode's mode 0400).
    default_permissions: bool = true,
    /// `max_read=`: the kernel's read cap for this mount.
    max_read: ?u32 = null,
    /// `user_id=` / `group_id=` on the root path (the process's by default).
    uid: u32 = 0,
    gid: u32 = 0,
    rootmode: u32 = DEFAULT_ROOTMODE,
    // ---- kernel: MS_* flags ----
    read_only: bool = false,
    nosuid: bool = false,
    nodev: bool = false,
    noexec: bool = false,
    noatime: bool = false,
    nodiratime: bool = false,
    dirsync: bool = false,
    sync: bool = false,
    // ---- daemon-level (INIT-reply overrides, TTLs, postures) ----
    /// `-o max_background=` (> 0): the INIT-reply override (the A0 lever
    /// restoring 256).
    max_background: ?u16 = null,
    congestion_threshold: ?u16 = null,
    ttls: Ttls = .{},
    /// `-o writeback` / `writeback_cache`: advertise `FUSE_WRITEBACK_CACHE`
    /// (refused on a read-only mount by the session).
    write_back: bool = false,
    /// `-o direct_device_true`: the data plane's direct-device posture.
    direct_device_true: bool = false,
    /// `-o stripe_dirs`: directory striping (rung 6).
    stripe_dirs: bool = false,
    /// `-o nonempty`: mount over a non-empty directory.
    nonempty: bool = false,

    pub fn fsName(self: MountOptions) []const u8 {
        return self.fs_name orelse DEFAULT_FSNAME;
    }

    /// The root path's `mount(2)` data: `fd=N,rootmode=40000,user_id=U,
    /// group_id=G[,subtype=S][,allow_other][,default_permissions][,max_read=N]`.
    /// `MS_*` postures ride `msFlags()`, not the string.
    pub fn writeKernelData(self: MountOptions, fd: i32, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("fd={d},rootmode={o},user_id={d},group_id={d}", .{ fd, self.rootmode, self.uid, self.gid });
        if (self.subtype) |s| try w.print(",subtype={s}", .{s});
        try self.writeSharedKernelOptions(w);
    }

    /// The `fusermount3 -o` string: `fsname=F[,subtype=S][,allow_other]
    /// [,default_permissions][,max_read=N][,ro][,nosuid][,nodev][,noexec]
    /// [,noatime][,nodiratime][,dirsync][,sync]`. fusermount3 supplies
    /// `fd/rootmode/user_id/group_id` itself and strips them if passed.
    pub fn writeFusermountOptions(self: MountOptions, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("fsname={s}", .{self.fsName()});
        if (self.subtype) |s| try w.print(",subtype={s}", .{s});
        try self.writeSharedKernelOptions(w);
        if (self.read_only) try w.writeAll(",ro");
        if (self.nosuid) try w.writeAll(",nosuid");
        if (self.nodev) try w.writeAll(",nodev");
        if (self.noexec) try w.writeAll(",noexec");
        if (self.noatime) try w.writeAll(",noatime");
        if (self.nodiratime) try w.writeAll(",nodiratime");
        if (self.dirsync) try w.writeAll(",dirsync");
        if (self.sync) try w.writeAll(",sync");
    }

    fn writeSharedKernelOptions(self: MountOptions, w: *std.Io.Writer) std.Io.Writer.Error!void {
        if (self.allow_other) try w.writeAll(",allow_other");
        if (self.default_permissions) try w.writeAll(",default_permissions");
        if (self.max_read) |n| try w.print(",max_read={d}", .{n});
    }

    /// `MS_*` for the root `mount(2)` path.
    pub fn msFlags(self: MountOptions) u32 {
        var f: u32 = linux.MS.NOSUID * @as(u32, @intFromBool(self.nosuid)) |
            linux.MS.NODEV * @as(u32, @intFromBool(self.nodev)) |
            linux.MS.NOEXEC * @as(u32, @intFromBool(self.noexec)) |
            linux.MS.RDONLY * @as(u32, @intFromBool(self.read_only)) |
            linux.MS.NOATIME * @as(u32, @intFromBool(self.noatime)) |
            linux.MS.NODIRATIME * @as(u32, @intFromBool(self.nodiratime)) |
            linux.MS.DIRSYNC * @as(u32, @intFromBool(self.dirsync));
        if (self.sync) f |= linux.MS.SYNCHRONOUS;
        return f;
    }
};

// ---------------------------------------------------------------------------
// The ONE option parser
// ---------------------------------------------------------------------------

/// Why an option refused the mount; formats as one line naming the option.
pub const Diagnostic = struct {
    option: []const u8,
    raw: []const u8,
    want: Want,

    pub const Want = union(enum) {
        /// An integer option's refusal from the ONE integer parser.
        int: knobs.Want,
        seconds,
        unknown_option,
        missing_value,
        unexpected_value,

        pub fn format(self: Want, w: *std.Io.Writer) std.Io.Writer.Error!void {
            switch (self) {
                .int => |k| try w.print("{f}", .{k}),
                .seconds => try w.writeAll("expected seconds as a canonical decimal with at most " ++ std.fmt.comptimePrint("{d}", .{MAX_FRACTION_DIGITS}) ++ " fraction digits (e.g. 1, 0.5, 2.250); no unit suffix, sign or exponent"),
                .unknown_option => try w.writeAll("not a squeezefs mount option"),
                .missing_value => try w.writeAll("requires =<value>"),
                .unexpected_value => try w.writeAll("takes no value"),
            }
        }
    };

    pub fn format(self: Diagnostic, w: *std.Io.Writer) std.Io.Writer.Error!void {
        if (self.raw.len == 0) {
            try w.print("-o {s}: {f}", .{ self.option, self.want });
        } else {
            try w.print("-o {s}='{s}' is invalid: {f}", .{ self.option, self.raw, self.want });
        }
    }
};

pub const Outcome = union(enum) {
    ok,
    invalid: Diagnostic,
};

/// Canonical decimal seconds → nanoseconds: `digits ['.' digits{1,9}]`,
/// nothing else. Total: null on any other spelling.
pub fn parseSecondsNs(text: []const u8) ?u64 {
    if (text.len == 0) return null;
    const dot = std.mem.indexOfScalar(u8, text, '.');
    const whole = if (dot) |d| text[0..d] else text;
    const frac = if (dot) |d| text[d + 1 ..] else "";
    if (whole.len == 0) return null;
    if (dot != null and (frac.len == 0 or frac.len > MAX_FRACTION_DIGITS)) return null;
    for (whole) |c| if (!std.ascii.isDigit(c)) return null;
    for (frac) |c| if (!std.ascii.isDigit(c)) return null;
    // Whole seconds are bounded so the ns product fits: u64 ns ≈ 584 years.
    const secs = std.fmt.parseInt(u64, whole, 10) catch return null;
    if (secs > std.math.maxInt(u64) / ns_per_s) return null;
    var frac_ns: u64 = 0;
    if (frac.len > 0) {
        frac_ns = std.fmt.parseInt(u64, frac, 10) catch return null;
        for (frac.len..MAX_FRACTION_DIGITS) |_| frac_ns *= 10;
    }
    return std.math.add(u64, secs * ns_per_s, frac_ns) catch null;
}

const Flag = struct { name: []const u8, field: []const u8, value: bool = true };

/// Value-less options → boolean fields.
const flags = [_]Flag{
    .{ .name = "ro", .field = "read_only" },
    .{ .name = "rw", .field = "read_only", .value = false },
    .{ .name = "nosuid", .field = "nosuid" },
    .{ .name = "nodev", .field = "nodev" },
    .{ .name = "noexec", .field = "noexec" },
    .{ .name = "noatime", .field = "noatime" },
    .{ .name = "nodiratime", .field = "nodiratime" },
    .{ .name = "dirsync", .field = "dirsync" },
    .{ .name = "sync", .field = "sync" },
    .{ .name = "allow_other", .field = "allow_other" },
    .{ .name = "default_permissions", .field = "default_permissions" },
    .{ .name = "writeback", .field = "write_back" },
    .{ .name = "writeback_cache", .field = "write_back" },
    .{ .name = "direct_device_true", .field = "direct_device_true" },
    .{ .name = "stripe_dirs", .field = "stripe_dirs" },
    .{ .name = "nonempty", .field = "nonempty" },
};

const TtlOption = struct { name: []const u8, class: TtlClass };
const ttl_options = [_]TtlOption{
    .{ .name = "attr_timeout", .class = .attr },
    .{ .name = "entry_timeout", .class = .entry },
    .{ .name = "dir_entry_timeout", .class = .dir_entry },
    .{ .name = "negative_timeout", .class = .negative },
};

/// Parse a libfuse-style `-o` string (`a,b=c,…`; empty items ignored) INTO
/// `opts` (so the env-seeded TTLs are the base the string overrides). The
/// first refusal stops the parse and names the option; the caller fails the
/// mount with the diagnostic. `fs_name`/`subtype` values are slices into
/// `text` (the caller keeps it alive for the mount).
pub fn parseOptionString(opts: *MountOptions, text: []const u8) Outcome {
    var it = std.mem.splitScalar(u8, text, ',');
    while (it.next()) |raw_item| {
        const item = std.mem.trim(u8, raw_item, " \t");
        if (item.len == 0) continue;
        const eq = std.mem.indexOfScalar(u8, item, '=');
        const name = if (eq) |e| item[0..e] else item;
        const value: ?[]const u8 = if (eq) |e| item[e + 1 ..] else null;
        switch (parseOne(opts, name, value)) {
            .ok => {},
            .invalid => |d| return .{ .invalid = d },
        }
    }
    return .ok;
}

fn parseOne(opts: *MountOptions, name: []const u8, value: ?[]const u8) Outcome {
    inline for (flags) |f| {
        if (std.mem.eql(u8, name, f.name)) {
            if (value != null) return refuse(name, value.?, .unexpected_value);
            @field(opts, f.field) = f.value;
            return .ok;
        }
    }
    inline for (ttl_options) |t| {
        if (std.mem.eql(u8, name, t.name)) {
            const v = value orelse return refuse(name, "", .missing_value);
            const ns = parseSecondsNs(v) orelse return refuse(name, v, .seconds);
            opts.ttls.set(t.class, ns);
            return .ok;
        }
    }
    if (std.mem.eql(u8, name, "fsname")) {
        const v = value orelse return refuse(name, "", .missing_value);
        if (v.len == 0) return refuse(name, "", .missing_value);
        opts.fs_name = v;
        return .ok;
    }
    if (std.mem.eql(u8, name, "subtype")) {
        const v = value orelse return refuse(name, "", .missing_value);
        if (v.len == 0) return refuse(name, "", .missing_value);
        opts.subtype = v;
        return .ok;
    }
    if (std.mem.eql(u8, name, "max_read")) {
        // The kernel floors max_read at 4096 (`max_t(unsigned, 4096, …)`): a
        // smaller value would be a silent correction — refuse instead.
        opts.max_read = switch (knobs.parseIntIn(u32, name, value, 4096, std.math.maxInt(u32))) {
            .value => |v| v,
            .absent => return refuse(name, "", .missing_value),
            .invalid => |d| return refuse(name, d.raw, .{ .int = d.want }),
        };
        return .ok;
    }
    if (std.mem.eql(u8, name, "max_background") or std.mem.eql(u8, name, "congestion_threshold")) {
        // A u16 wire field; 0 would mean "policy" and is refused as a value.
        const v: u16 = switch (knobs.parseIntIn(u16, name, value, 1, std.math.maxInt(u16))) {
            .value => |v| v,
            .absent => return refuse(name, "", .missing_value),
            .invalid => |d| return refuse(name, d.raw, .{ .int = d.want }),
        };
        if (name[0] == 'm') opts.max_background = v else opts.congestion_threshold = v;
        return .ok;
    }
    return refuse(name, value orelse "", .unknown_option);
}

fn refuse(option: []const u8, raw: []const u8, want: Diagnostic.Want) Outcome {
    return .{ .invalid = .{ .option = option, .raw = raw, .want = want } };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

fn fmtDiag(buf: []u8, o: Outcome) ![]const u8 {
    return switch (o) {
        .invalid => |d| std.fmt.bufPrint(buf, "{f}", .{d}),
        .ok => error.TestExpectedInvalid,
    };
}

test "seconds: canonical decimal with an optional ≤ 9-digit fraction; `5s`, signs, exponents, double dots, a bare dot and 10 fraction digits refuse" {
    try testing.expectEqual(@as(?u64, 1_000_000_000), parseSecondsNs("1"));
    try testing.expectEqual(@as(?u64, 0), parseSecondsNs("0"));
    try testing.expectEqual(@as(?u64, 500_000_000), parseSecondsNs("0.5"));
    try testing.expectEqual(@as(?u64, 2_250_000_000), parseSecondsNs("2.250"));
    try testing.expectEqual(@as(?u64, 1), parseSecondsNs("0.000000001"));
    try testing.expectEqual(@as(?u64, 30_000_000_000), parseSecondsNs("30"));
    try testing.expectEqual(@as(?u64, 1_000_000_000), parseSecondsNs("01.000"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("5s"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("-1"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("+1"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("1e3"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("1.5.2"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("."));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs(".5"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("1."));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs(""));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs(" 1"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("0.0000000001"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("nan"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("inf"));
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("99999999999999999999"));
    // The u64-ns ceiling: 18_446_744_073 s fits, one more does not.
    try testing.expect(parseSecondsNs("18446744073") != null);
    try testing.expectEqual(@as(?u64, null), parseSecondsNs("18446744074"));
}

test "TTL options reach the four classes through the ONE parser; junk refuses the mount naming the option (Q11); 0 disables negatives" {
    var o: MountOptions = .{};
    try testing.expectEqual(Outcome.ok, parseOptionString(&o, "attr_timeout=0.25,entry_timeout=2,dir_entry_timeout=30,negative_timeout=0"));
    try testing.expectEqual(@as(u64, 250_000_000), o.ttls.attr_ns);
    try testing.expectEqual(@as(u64, 2_000_000_000), o.ttls.entry_ns);
    try testing.expectEqual(@as(u64, 30_000_000_000), o.ttls.dir_entry_ns);
    try testing.expectEqual(@as(u64, 0), o.ttls.negative_ns);
    try testing.expect(!o.ttls.negativeEnabled());
    try testing.expectEqual(@as(u64, 30_000_000_000), o.ttls.entryTtlFor(true));
    try testing.expectEqual(@as(u64, 2_000_000_000), o.ttls.entryTtlFor(false));
    try testing.expectEqual(@as(u64, 250_000_000), o.ttls.get(.attr));
    var buf: [256]u8 = undefined;
    var bad: MountOptions = .{};
    try testing.expectEqualStrings("-o attr_timeout='5s' is invalid: expected seconds as a canonical decimal with at most 9 fraction digits (e.g. 1, 0.5, 2.250); no unit suffix, sign or exponent", try fmtDiag(&buf, parseOptionString(&bad, "attr_timeout=5s")));
    try testing.expect(parseOptionString(&bad, "entry_timeout=-1") == .invalid);
    try testing.expect(parseOptionString(&bad, "negative_timeout=1e2") == .invalid);
    try testing.expectEqualStrings("-o dir_entry_timeout: requires =<value>", try fmtDiag(&buf, parseOptionString(&bad, "dir_entry_timeout")));
    // A refused option leaves the earlier ones applied and names the offender; defaults untouched.
    try testing.expectEqual(DEFAULT_TTL_NS, bad.ttls.attr_ns);
}

test "integer options go through the ONE integer parser: max_background/congestion_threshold u16 ≥ 1, max_read ≥ 4096; 0, junk and out-of-range refuse naming the range" {
    var o: MountOptions = .{};
    try testing.expectEqual(Outcome.ok, parseOptionString(&o, "max_background=256,congestion_threshold=192,max_read=131072"));
    try testing.expectEqual(@as(?u16, 256), o.max_background);
    try testing.expectEqual(@as(?u16, 192), o.congestion_threshold);
    try testing.expectEqual(@as(?u32, 131072), o.max_read);
    var buf: [256]u8 = undefined;
    try testing.expectEqualStrings("-o max_background='0' is invalid: expected an integer in 1..=65535", try fmtDiag(&buf, parseOptionString(&o, "max_background=0")));
    try testing.expectEqualStrings("-o max_background='65536' is invalid: expected an integer in 1..=65535", try fmtDiag(&buf, parseOptionString(&o, "max_background=65536")));
    try testing.expectEqualStrings("-o congestion_threshold='lots' is invalid: expected an integer in 1..=65535", try fmtDiag(&buf, parseOptionString(&o, "congestion_threshold=lots")));
    try testing.expectEqualStrings("-o max_read='512' is invalid: expected an integer in 4096..=4294967295", try fmtDiag(&buf, parseOptionString(&o, "max_read=512")));
    try testing.expectEqualStrings("-o max_background: requires =<value>", try fmtDiag(&buf, parseOptionString(&o, "max_background")));
    try testing.expect(parseOptionString(&o, "max_background=12.5") == .invalid);
    // The earlier values survived the refusals.
    try testing.expectEqual(@as(?u16, 256), o.max_background);
}

test "flags and names: ro/rw/nosuid/nodev/noexec/noatime/nodiratime/dirsync/sync/allow_other/default_permissions/writeback/direct_device_true/stripe_dirs/nonempty; fsname and subtype; a value on a flag, an empty name, an unknown option refuse" {
    var o: MountOptions = .{};
    try testing.expectEqual(Outcome.ok, parseOptionString(&o, "ro,nosuid, nodev ,noexec,noatime,nodiratime,dirsync,sync,allow_other,writeback,direct_device_true,stripe_dirs,nonempty,fsname=/dev/nvme0n1,subtype=sqz,,"));
    try testing.expect(o.read_only and o.nosuid and o.nodev and o.noexec and o.noatime and o.nodiratime and o.dirsync and o.sync);
    try testing.expect(o.allow_other and o.write_back and o.direct_device_true and o.stripe_dirs and o.nonempty);
    try testing.expectEqualStrings("/dev/nvme0n1", o.fs_name.?);
    try testing.expectEqualStrings("sqz", o.subtype.?);
    try testing.expectEqual(Outcome.ok, parseOptionString(&o, "rw,writeback_cache"));
    try testing.expect(!o.read_only);
    var buf: [256]u8 = undefined;
    try testing.expectEqualStrings("-o ro='1' is invalid: takes no value", try fmtDiag(&buf, parseOptionString(&o, "ro=1")));
    try testing.expectEqualStrings("-o fsname: requires =<value>", try fmtDiag(&buf, parseOptionString(&o, "fsname=")));
    try testing.expectEqualStrings("-o subtype: requires =<value>", try fmtDiag(&buf, parseOptionString(&o, "subtype")));
    try testing.expectEqualStrings("-o interception: not a squeezefs mount option", try fmtDiag(&buf, parseOptionString(&o, "interception")));
    try testing.expectEqualStrings("-o max_write='1048576' is invalid: not a squeezefs mount option", try fmtDiag(&buf, parseOptionString(&o, "max_write=1048576")));
    try testing.expect(parseOptionString(&o, "async_read") == .invalid);
    // default_permissions is the shipped posture and cannot be turned off by the string.
    try testing.expect(o.default_permissions);
}

test "fusermount3 string: fsname and subtype are independent; subtype rides without fsname (default source `squeezefs`); no subtype emits none; daemon options never reach it" {
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    var o: MountOptions = .{};
    try o.writeFusermountOptions(&w);
    try testing.expectEqualStrings("fsname=squeezefs,subtype=squeezefs,default_permissions", w.buffered());

    w = .fixed(&buf);
    try testing.expectEqual(Outcome.ok, parseOptionString(&o, "fsname=/dev/sdb,allow_other,ro,nosuid,nodev,noexec,noatime,nodiratime,dirsync,sync,max_read=65536,max_background=256,congestion_threshold=192,attr_timeout=5,entry_timeout=5,dir_entry_timeout=5,negative_timeout=0,direct_device_true,stripe_dirs,writeback"));
    try o.writeFusermountOptions(&w);
    try testing.expectEqualStrings("fsname=/dev/sdb,subtype=squeezefs,allow_other,default_permissions,max_read=65536,ro,nosuid,nodev,noexec,noatime,nodiratime,dirsync,sync", w.buffered());
    for ([_][]const u8{ "max_background", "congestion_threshold", "timeout", "direct_device_true", "stripe_dirs", "writeback" }) |forbidden| {
        try testing.expect(std.mem.indexOf(u8, w.buffered(), forbidden) == null);
    }

    w = .fixed(&buf);
    o.subtype = null;
    try o.writeFusermountOptions(&w);
    try testing.expectEqualStrings("fsname=/dev/sdb,allow_other,default_permissions,max_read=65536,ro,nosuid,nodev,noexec,noatime,nodiratime,dirsync,sync", w.buffered());
}

test "root mount(2) data: fd, octal rootmode, uid/gid, subtype, the shared kernel options; MS flags carry the postures" {
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    var o: MountOptions = .{ .uid = 1000, .gid = 1001, .allow_other = true, .max_read = 1 << 20 };
    try o.writeKernelData(7, &w);
    try testing.expectEqualStrings("fd=7,rootmode=40000,user_id=1000,group_id=1001,subtype=squeezefs,allow_other,default_permissions,max_read=1048576", w.buffered());
    try testing.expectEqual(@as(u32, 0), o.msFlags());
    o.read_only = true;
    o.nosuid = true;
    o.nodev = true;
    o.noexec = true;
    try testing.expectEqual(@as(u32, linux.MS.RDONLY | linux.MS.NOSUID | linux.MS.NODEV | linux.MS.NOEXEC), o.msFlags());
    o.noatime = true;
    o.nodiratime = true;
    o.dirsync = true;
    o.sync = true;
    try testing.expectEqual(@as(u32, linux.MS.RDONLY | linux.MS.NOSUID | linux.MS.NODEV | linux.MS.NOEXEC | linux.MS.NOATIME | linux.MS.NODIRATIME | linux.MS.DIRSYNC | linux.MS.SYNCHRONOUS), o.msFlags());
    // MS_* never appear in the data string.
    w = .fixed(&buf);
    try o.writeKernelData(7, &w);
    try testing.expectEqualStrings("fd=7,rootmode=40000,user_id=1000,group_id=1001,subtype=squeezefs,allow_other,default_permissions,max_read=1048576", w.buffered());
}

test "TTL precedence: 1 s default → env _TTL_MS rows → -o *_timeout=; the env layer reads the registry rows" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const d = Ttls.fromEnv(&env);
    try testing.expectEqual(DEFAULT_TTL_NS, d.attr_ns);
    try testing.expectEqual(DEFAULT_TTL_NS, d.entry_ns);
    try testing.expectEqual(DEFAULT_TTL_NS, d.dir_entry_ns);
    try testing.expectEqual(DEFAULT_TTL_NS, d.negative_ns);
    try testing.expectEqual(@as(u64, 1000), knobs.intDefault("SQUEEZEFS_FUSE_ATTR_TTL_MS", u64));
    try env.put("SQUEEZEFS_FUSE_ATTR_TTL_MS", "250");
    try env.put("SQUEEZEFS_FUSE_ENTRY_TTL_MS", "0");
    try env.put("SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", "60000");
    try env.put("SQUEEZEFS_FUSE_NEGATIVE_TTL_MS", "5");
    const e = Ttls.fromEnv(&env);
    try testing.expectEqual(@as(u64, 250_000_000), e.attr_ns);
    try testing.expectEqual(@as(u64, 0), e.entry_ns);
    try testing.expectEqual(@as(u64, 60_000_000_000), e.dir_entry_ns);
    try testing.expectEqual(@as(u64, 5_000_000), e.negative_ns);
    // The string overrides the env for the classes it names and leaves the rest.
    var o: MountOptions = .{ .ttls = e };
    try testing.expectEqual(Outcome.ok, parseOptionString(&o, "attr_timeout=1.5"));
    try testing.expectEqual(@as(u64, 1_500_000_000), o.ttls.attr_ns);
    try testing.expectEqual(@as(u64, 0), o.ttls.entry_ns);
    try testing.expectEqual(@as(u64, 60_000_000_000), o.ttls.dir_entry_ns);
    // The env row refuses a fraction and a unit at the gate like every knob.
    try testing.expect(knobs.parseIntIn(u64, "SQUEEZEFS_FUSE_ATTR_TTL_MS", "1.5", 0, knobs.MS_MAX) == .invalid);
    try testing.expect(knobs.parseIntIn(u64, "SQUEEZEFS_FUSE_ATTR_TTL_MS", "5s", 0, knobs.MS_MAX) == .invalid);
}

test "the parser is total on arbitrary option strings" {
    var prng = std.Random.DefaultPrng.init(0x0A7);
    const r = prng.random();
    var buf: [256]u8 = undefined;
    const alphabet = "abcdefghijklmnopqrstuvwxyz_=,.0123456789 s-+e";
    for (0..20_000) |_| {
        const len = r.intRangeAtMost(usize, 0, buf.len);
        for (buf[0..len]) |*c| c.* = alphabet[r.intRangeLessThan(usize, 0, alphabet.len)];
        var o: MountOptions = .{};
        _ = parseOptionString(&o, buf[0..len]);
        _ = parseSecondsNs(buf[0..len]);
    }
}
