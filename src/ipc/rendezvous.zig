//! The rendezvous (charter §7; spec `docs/porting/il_session.md` §2), the
//! PORTABLE half: the `il0` blob the daemon synthesizes, the nonce
//! lifecycle, the daemon's HELLO ladder as a pure verdict over decoded
//! facts, THE FD SCREEN as a pure rule set over an fd's facts, the client's
//! authentication predicates, the socket-dir resolution, the heartbeat
//! watch. `rendezvous_linux.zig` gathers the facts (sockets, `SCM_RIGHTS`,
//! `F_GETFL`, `statx`, `SO_PEERCRED`) and runs these verdicts.
//!
//! Security model (spec §2.7 "why forgery fails"): fd possession is the
//! sole authorizing credential — to bind ino X you must present a
//! description whose `fstat` shows `(st_dev == mount, st_ino == X)` AND
//! survives the screen; only a permission-checked open produces that; the
//! daemon reads the DESCRIPTION's flags, so a client cannot claim `O_RDWR`
//! on an `O_RDONLY` description. `SO_PEERCRED` is for logs and caps, never
//! the authorizer. The nonce is anti-replay for stale traces only.

const std = @import("std");
const layout = @import("layout.zig");
const wire = @import("wire.zig");
const knobs = @import("../core/knobs.zig");
const derive = @import("../core/derive.zig");

pub const Il0Blob = wire.Il0Blob;
pub const RefuseClass = wire.RefuseClass;

/// Anti-replay horizon for stale traces; current + previous accepted so a
/// rotation never races a live handshake. A blob read at `t` is therefore
/// valid for a HELLO until the SECOND rotation after `t`: 60–120 s.
pub const NONCE_TTL_NS: u64 = 60 * std.time.ns_per_s;
/// An unestablished peer silent this long has given up; matched to the
/// client's `SO_RCVTIMEO` (`CTL_RECV_TIMEOUT_NS`).
pub const CTL_HANDSHAKE_TIMEOUT_NS: u64 = 10 * std.time.ns_per_s;
pub const CTL_RECV_TIMEOUT_NS: u64 = 10 * std.time.ns_per_s;
/// The ctl socket's liveness poll so teardown is bounded even if a sever
/// is missed.
pub const CTL_RECV_POLL_NS: u64 = 500 * std.time.ns_per_ms;
/// A leaked idle session holds one arena of the cap; 300 s ≫ any open→read
/// gap, ≪ the per-uid cap's time to fill under a fork-bomb (decision 0002
/// ledger Q12: a constant, not a knob).
pub const IDLE_REAP_NS: u64 = 300 * std.time.ns_per_s;

// ---------------------------------------------------------------------------
// The nonce.
// ---------------------------------------------------------------------------

pub const Nonce = [wire.NONCE_LEN]u8;

/// One random nonce per daemon instance, rotated LAZILY (on every blob
/// synthesis and every HELLO — control-plane cadence), multi-use within
/// its TTL (concurrent handshakes need no serialization), accepted =
/// current ∨ previous. The caller serializes (one host, one ctl thread).
pub const NonceState = struct {
    current: Nonce,
    previous: ?Nonce = null,
    rotated_at_ns: u64,

    /// The entropy source: `rendezvous_linux.fillNonce` (`getrandom(2)`) in
    /// the product; a PRNG in the portable tests.
    pub const Fill = *const fn (*Nonce) void;

    pub fn init(fill: Fill, now_ns: u64) NonceState {
        var n: Nonce = undefined;
        fill(&n);
        return .{ .current = n, .rotated_at_ns = now_ns };
    }

    /// Rotates once the TTL elapsed; returns whether it did.
    pub fn rotateIfStale(self: *NonceState, fill: Fill, now_ns: u64) bool {
        if (now_ns - self.rotated_at_ns < NONCE_TTL_NS) return false;
        self.previous = self.current;
        fill(&self.current);
        self.rotated_at_ns = now_ns;
        return true;
    }

    pub fn accepts(self: *const NonceState, nonce: *const Nonce) bool {
        if (std.mem.eql(u8, &self.current, nonce)) return true;
        if (self.previous) |p| return std.mem.eql(u8, &p, nonce);
        return false;
    }
};

// ---------------------------------------------------------------------------
// The blob and the socket names.
// ---------------------------------------------------------------------------

pub const Identity = struct {
    build_commit: []const u8,
    /// The abstract name WITHOUT the leading NUL.
    socket_name: []const u8,
    /// Empty when the path rung is disabled or degraded.
    socket_path: []const u8 = "",
};

pub fn buildBlob(id: Identity, nonce: *const Nonce) wire.EncodeError!Il0Blob {
    var b: Il0Blob = .{};
    try wire.setField(&b.build_commit, id.build_commit);
    try wire.setField(&b.socket, id.socket_name);
    try wire.setField(&b.socket_path, id.socket_path);
    b.nonce = nonce.*;
    return b;
}

/// `sqz-il0-<pid>-<rand u32 hex>`: pid + random so stale same-name residue
/// in the socket dir is OUR corpse and is replaced.
pub const SOCKET_NAME_MAX = "sqz-il0-".len + 10 + 1 + 8;
pub fn socketName(buf: *[SOCKET_NAME_MAX]u8, pid: u32, rand: u32) []const u8 {
    return std.fmt.bufPrint(buf, "sqz-il0-{d}-{x:0>8}", .{ pid, rand }) catch unreachable;
}

/// The path listener's directory: `SQUEEZEFS_IPC_SOCKET_DIR` (`none`
/// disables) else `/run/squeezefs` (euid 0) else `$XDG_RUNTIME_DIR/
/// squeezefs` else `/tmp/squeezefs-il-<uid>`. Null = no path rung.
pub const DirBuf = [std.fs.max_path_bytes]u8;
pub fn socketDir(buf: *DirBuf, knob: ?[]const u8, euid: u32, xdg_runtime_dir: ?[]const u8) ?[]const u8 {
    if (knob) |k| {
        if (std.mem.eql(u8, k, "none")) return null;
        if (k.len >= buf.len) return null;
        @memcpy(buf[0..k.len], k);
        return buf[0..k.len];
    }
    if (euid == 0) return std.fmt.bufPrint(buf, "/run/squeezefs", .{}) catch unreachable;
    if (xdg_runtime_dir) |x| {
        if (x.len > 0) return std.fmt.bufPrint(buf, "{s}/squeezefs", .{x}) catch return null;
    }
    return std.fmt.bufPrint(buf, "/tmp/squeezefs-il-{d}", .{euid}) catch unreachable;
}

pub fn socketDirFromEnv(buf: *DirBuf, env: *const std.process.Environ.Map, euid: u32) ?[]const u8 {
    return socketDir(buf, knobs.parseStr(env.get("SQUEEZEFS_IPC_SOCKET_DIR")), euid, knobs.parseStr(env.get("XDG_RUNTIME_DIR")));
}

/// A rendezvous directory is trusted iff it is owned by root or the mount
/// owner (daemon side: the daemon's own euid) AND is neither group- nor
/// world-writable. Both sides run the same predicate: the daemon refuses
/// to BIND there (degrading to abstract-only), the client refuses to
/// CONNECT there (`UntrustedRendezvous`, before any fd leaves).
pub fn dirTrusted(st_uid: u32, st_mode: u32, owner_uid: u32) bool {
    return (st_uid == 0 or st_uid == owner_uid) and st_mode & 0o022 == 0;
}

/// The direct arm's screen (charter §7 "The direct arm", decision 0003
/// §AX): a session is granted the direct arm iff its peer could open EVERY
/// data device node for reading — root, or the owner/group/other read bit
/// that applies to the peer's uid and primary gid (supplementary groups are
/// not consulted: a refusal there costs the ring path, never a grant the
/// kernel would have refused, since the client's own `open(2)` is the final
/// word). The trust this grants is whole-device: a direct client reads any
/// block of the volume, so the mount owner opts in (`-o interception=direct`)
/// for clients it already trusts with the device.
pub fn deviceReadable(st_mode: u32, st_uid: u32, st_gid: u32, peer_uid: u32, peer_gid: u32) bool {
    if (peer_uid == 0) return true;
    if (st_uid == peer_uid) return st_mode & 0o400 != 0;
    if (st_gid == peer_gid) return st_mode & 0o040 != 0;
    return st_mode & 0o004 != 0;
}

/// The write arm's screen: the write bit that is the peer's.
pub fn deviceWritable(st_mode: u32, st_uid: u32, st_gid: u32, peer_uid: u32, peer_gid: u32) bool {
    if (peer_uid == 0) return true;
    if (st_uid == peer_uid) return st_mode & 0o200 != 0;
    if (st_gid == peer_gid) return st_mode & 0o020 != 0;
    return st_mode & 0o002 != 0;
}

/// Rung 2: the daemon's uid must be root or the mount owner. Deliberately
/// NO "or our own uid" disjunct: the conservative predicate only costs
/// passthrough on a mount served by a third-party service account.
pub fn peerTrusted(peer_uid: u32, mount_uid: u32) bool {
    return peer_uid == 0 or peer_uid == mount_uid;
}

// ---------------------------------------------------------------------------
// THE FD SCREEN — the security boundary, as pure rules over an fd's facts.
// ---------------------------------------------------------------------------

pub const AccessMode = enum(u2) { rdonly = 0, wronly = 1, rdwr = 2, invalid = 3 };

/// What `F_GETFL` says about the DESCRIPTION (never the client's claim).
pub const OpenFlags = struct {
    accmode: AccessMode,
    path: bool = false,
    append: bool = false,
    sync: bool = false,
    dsync: bool = false,
    tmpfile: bool = false,
    direct: bool = false,
};

pub const FileKind = enum { regular, directory, other };

pub const FdFacts = struct {
    /// Null when `F_GETFL` failed.
    flags: ?OpenFlags,
    /// Null when `fstat` failed.
    stat: ?struct { kind: FileKind, dev: u64, ino: u64, nlink: u32 },
};

pub const ScreenOk = struct {
    ino: u64,
    rights: wire.Rights,
};

pub const ScreenVerdict = union(enum) {
    ok: ScreenOk,
    refuse: RefuseClass,
};

/// Run on every received fd (the HELLO credential and every BIND). The
/// HELLO credential of an SDK client may be the mount root DIRECTORY
/// (`admit_directory`); BIND and the shim's credential require a regular
/// file. `expected_dev` null = the mount's device is unresolved (refuses
/// `mount`; the port resolves it before the host arms, so this is a guard).
pub fn screenFd(f: FdFacts, expected_dev: ?u64, admit_directory: bool) ScreenVerdict {
    // 1. `F_GETFL`; `O_PATH` — load-bearing: an O_PATH fd is obtainable with
    //    search-only permission and its access-mode bits read O_RDONLY (0);
    //    a naive mode check would grant ring reads of a file the uid cannot
    //    read (cross-user disclosure on --allow-other).
    const fl = f.flags orelse return .{ .refuse = .flags };
    if (fl.path) return .{ .refuse = .flags };
    // 2. `fstat`; a regular file (directories, devices, FIFOs, sockets refuse).
    const st = f.stat orelse return .{ .refuse = .flags };
    switch (st.kind) {
        .regular => {},
        .directory => if (!admit_directory) return .{ .refuse = .flags },
        .other => return .{ .refuse = .flags },
    }
    // 3. The mount's device.
    const dev = expected_dev orelse return .{ .refuse = .mount };
    if (st.dev != dev) return .{ .refuse = .mount };
    // 4. Append needs an atomic size authority round trip; sync needs
    //    per-op barriers the kernel path already provides; `st_nlink == 0`
    //    also refuses open-then-unlinked fds — conservatively correct,
    //    passthrough serves them.
    if (fl.append or fl.sync or fl.dsync or fl.tmpfile or st.nlink == 0) return .{ .refuse = .flags };
    // 5. Access mode ⇒ rights BOTH directions (a wrong-direction op later
    //    completes -EBADF like the kernel).
    const rights: wire.Rights = switch (fl.accmode) {
        .rdonly => .{ .read = true },
        .wronly => .{ .write = true },
        .rdwr => .{ .read = true, .write = true },
        .invalid => return .{ .refuse = .rights },
    };
    var r = rights;
    r.odirect = fl.direct;
    return .{ .ok = .{ .ino = st.ino, .rights = r } };
}

// ---------------------------------------------------------------------------
// The daemon's HELLO ladder as a pure verdict (spec §2.6 steps 2–8; step 1
// is the caller's tag dispatch, steps 9–11 are the host's acts).
// ---------------------------------------------------------------------------

pub const Policy = struct {
    /// `-o interception`.
    armed: bool,
    our_commit: []const u8,
    /// `SQUEEZEFS_TEST_IPC_ALLOW_DIRTY`.
    allow_dirty: bool = false,
    default_geometry: layout.Geometry = layout.Geometry.default,
    arena_cap_bytes: u64,
    arena_in_use_bytes: u64 = 0,
    /// The R5 shed hook: refuse NEW sessions while in use > target.
    shed_target_bytes: ?u64 = null,
    per_uid_session_cap: u64,
    /// The peer uid's live sessions.
    uid_sessions: u64 = 0,
};

pub const PeerCred = struct { pid: u32, uid: u32, gid: u32 };

pub const Admission = struct {
    kind: wire.ClientKind,
    geometry: layout.Geometry,
    footprint_bytes: u64,
    /// A degenerate identity admitted by the test seam (counted).
    dev_override: bool,
    credential: ScreenOk,
};

pub const Verdict = union(enum) {
    admit: Admission,
    refuse: RefuseClass,
};

/// Steps 2–8, in the ladder's order; the first refusal wins.
pub fn admitHello(p: Policy, hello: *const wire.Hello, credential: ?FdFacts, mount_dev: ?u64, peer: ?PeerCred, nonces: *const NonceState) Verdict {
    // 2. Not armed ⇒ Disabled before any fd screen.
    if (!p.armed) return .{ .refuse = .disabled };
    // 3. The coarse ABI, the client kind, then the kind's version law.
    if (hello.abi != wire.IPC_ABI) return .{ .refuse = .version };
    const kind: wire.ClientKind = switch (hello.client_kind) {
        0 => .shim,
        1 => .sdk,
        else => return .{ .refuse = .version },
    };
    var dev_override = false;
    switch (kind) {
        .shim => {
            const theirs = hello.buildCommit();
            if (!std.mem.eql(u8, theirs, p.our_commit)) return .{ .refuse = .version };
            // 4. A degenerate identity on either side refuses unless the seam admits it.
            if (wire.buildCommitDegenerate(theirs) or wire.buildCommitDegenerate(p.our_commit)) {
                if (!p.allow_dirty) return .{ .refuse = .version };
                dev_override = true;
            }
        },
        .sdk => {
            // The SDK pairs on the frozen-subset window, never on the commit.
            var accepted = false;
            for (wire.sdk_abi_accepted) |a| accepted = accepted or a == hello.sdk_abi;
            if (!accepted) return .{ .refuse = .version };
        },
        _ => unreachable,
    }
    // 5. Nonce freshness.
    if (!nonces.accepts(&hello.nonce)) return .{ .refuse = .nonce };
    // 6. `SO_PEERCRED` unreadable ⇒ Internal (for logs/caps; never the authorizer).
    if (peer == null) return .{ .refuse = .internal };
    // 7. No fd ⇒ Flags; the screen's own class.
    const facts = credential orelse return .{ .refuse = .flags };
    const cred = switch (screenFd(facts, mount_dev, kind == .sdk)) {
        .ok => |ok| ok,
        .refuse => |c| return .{ .refuse = c },
    };
    // 8. Budget: the geometry (the SDK's request clamped under the cap),
    //    the shed target, the cap, the per-uid cap.
    const geometry = grantGeometry(p, hello, kind);
    const footprint = (layout.SessionLayout.compute(geometry) catch return .{ .refuse = .internal }).total_bytes;
    if (p.shed_target_bytes) |shed| if (p.arena_in_use_bytes > shed) return .{ .refuse = .budget };
    if (p.arena_in_use_bytes + footprint > p.arena_cap_bytes) return .{ .refuse = .budget };
    if (p.uid_sessions >= p.per_uid_session_cap) return .{ .refuse = .budget };
    return .{ .admit = .{ .kind = kind, .geometry = geometry, .footprint_bytes = footprint, .dev_override = dev_override, .credential = cred } };
}

/// The SDK's geometry request clamped under the cap: `arena = min(req, MAX,
/// cap − in_use)` rounded down to a page; below `max_op` ⇒ the default
/// ("refusal degrades to the default geometry, never fails the session");
/// `slots = clamp(req, 1, ring_entries)`. Kind shim gets the default.
pub fn grantGeometry(p: Policy, hello: *const wire.Hello, kind: wire.ClientKind) layout.Geometry {
    var g = p.default_geometry;
    if (kind != .sdk) return g;
    if (hello.arena_bytes_req != 0) {
        const room = p.arena_cap_bytes -| p.arena_in_use_bytes;
        var granted: u64 = @min(hello.arena_bytes_req, @min(layout.MAX_ARENA_BYTES, room));
        granted = derive.alignDown(granted, layout.PAGE_BYTES);
        if (granted >= g.max_op_bytes) g.arena_bytes = granted;
    }
    if (hello.slots_req != 0) g.slots = @intCast(std.math.clamp(hello.slots_req, 1, g.ring_entries));
    return g;
}

/// The client's pre-check before connecting (skips the doomed round trip;
/// the daemon enforces the same law authoritatively).
pub fn preCheck(blob: *const Il0Blob, my_commit: []const u8, kind: wire.ClientKind, allow_dirty: bool) ?RefuseClass {
    if (blob.abi != wire.IPC_ABI) return .version;
    if (kind == .shim) {
        const theirs = blob.buildCommit();
        if (!std.mem.eql(u8, theirs, my_commit)) return .version;
        if ((wire.buildCommitDegenerate(theirs) or wire.buildCommitDegenerate(my_commit)) and !allow_dirty) return .version;
    }
    return null;
}

pub fn allowDirtyFromEnv(env: *const std.process.Environ.Map) bool {
    return knobs.boolKnob("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", env);
}

// ---------------------------------------------------------------------------
// The client's heartbeat watch (decision 0003 §F): the daemon stamps
// `header.heartbeat` every service pass top and park exit; a client parked
// on an op escalates 1 s unchanged → socket probe → poison; the per-op
// deadline is the last rung.
// ---------------------------------------------------------------------------

/// The futex slice a parked client re-checks poison and the heartbeat at.
pub const WAIT_SLICE_NS: u64 = 50 * std.time.ns_per_ms;
/// 20 × 50 ms slices with the word unchanged: the daemon's service pass
/// stamps at least every park (≤ 5 ms) and every tick, so a second of
/// silence is a dead or stopped daemon, never a busy one.
pub const HEARTBEAT_STALE_NS: u64 = 20 * WAIT_SLICE_NS;
/// The per-op deadline that poisons: the LAST rung.
pub const OP_TIMEOUT_NS: u64 = 30 * std.time.ns_per_s;

pub const HeartbeatWatch = struct {
    last: u64,
    since_ns: u64,

    pub const Step = enum { alive, probe };

    pub fn init(heartbeat: u64, now_ns: u64) HeartbeatWatch {
        return .{ .last = heartbeat, .since_ns = now_ns };
    }

    /// Feed the current word; `.probe` once it has not moved for the stale
    /// bound (the caller probes the ctl socket; EOF/ECONNRESET ⇒ poison).
    pub fn observe(self: *HeartbeatWatch, heartbeat: u64, now_ns: u64) Step {
        if (heartbeat != self.last) {
            self.last = heartbeat;
            self.since_ns = now_ns;
            return .alive;
        }
        return if (now_ns - self.since_ns >= HEARTBEAT_STALE_NS) .probe else .alive;
    }
};

/// `SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS` (0 = the constant).
pub fn opTimeoutFromEnv(env: *const std.process.Environ.Map) u64 {
    const ms = knobs.intKnob("SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS", u64, env);
    return if (ms == 0) OP_TIMEOUT_NS else ms * std.time.ns_per_ms;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

var test_prng = std.Random.DefaultPrng.init(0xbeef);
fn testFill(n: *Nonce) void {
    test_prng.random().bytes(n);
}

test "nonce lifecycle: multi-use within the TTL; lazy rotation keeps the previous; a blob read at t is accepted until the SECOND rotation after t (60–120 s); older refuses" {
    const r: NonceState.Fill = &testFill;
    var n = NonceState.init(r, 0);
    const a = n.current;
    try testing.expect(n.accepts(&a));
    try testing.expect(n.accepts(&a));
    try testing.expect(!n.rotateIfStale(r, NONCE_TTL_NS - 1));
    try testing.expect(n.rotateIfStale(r, NONCE_TTL_NS));
    const b = n.current;
    try testing.expect(!std.mem.eql(u8, &a, &b));
    try testing.expect(n.accepts(&a));
    try testing.expect(n.accepts(&b));
    try testing.expect(n.rotateIfStale(r, 2 * NONCE_TTL_NS));
    try testing.expect(!n.accepts(&a));
    try testing.expect(n.accepts(&b));
    var zero: Nonce = @splat(0);
    try testing.expect(!n.accepts(&zero));
    zero = n.current;
    try testing.expect(n.accepts(&zero));
}

test "the blob is built from the identity + nonce and decodes back; socket names embed pid + random; the dir resolution ladder" {
    const n = NonceState.init(&testFill, 0);
    var name_buf: [SOCKET_NAME_MAX]u8 = undefined;
    const name = socketName(&name_buf, 4242, 0xdeadbeef);
    try testing.expectEqualStrings("sqz-il0-4242-deadbeef", name);
    const blob = try buildBlob(.{ .build_commit = "0123456789ab", .socket_name = name, .socket_path = "/run/squeezefs/sqz-il0-4242-deadbeef.sock" }, &n.current);
    const d = try Il0Blob.decode(blob.bytes());
    try testing.expectEqualStrings("0123456789ab", d.buildCommit());
    try testing.expectEqualStrings(name, d.socketName());
    try testing.expect(std.mem.eql(u8, &n.current, &d.nonce));
    try testing.expectEqual(@as(?RefuseClass, null), preCheck(&d, "0123456789ab", .shim, false));
    try testing.expectEqual(@as(?RefuseClass, .version), preCheck(&d, "0123456789ac", .shim, false));
    try testing.expectEqual(@as(?RefuseClass, null), preCheck(&d, "whatever", .sdk, false));
    var dirty = blob;
    try wire.setField(&dirty.build_commit, "0123456789ab-dirty");
    try testing.expectEqual(@as(?RefuseClass, .version), preCheck(&dirty, "0123456789ab-dirty", .shim, false));
    try testing.expectEqual(@as(?RefuseClass, null), preCheck(&dirty, "0123456789ab-dirty", .shim, true));
    var buf: DirBuf = undefined;
    try testing.expectEqual(@as(?[]const u8, null), socketDir(&buf, "none", 1000, null));
    try testing.expectEqualStrings("/opt/sqz", socketDir(&buf, "/opt/sqz", 1000, null).?);
    try testing.expectEqualStrings("/run/squeezefs", socketDir(&buf, null, 0, "/run/user/0").?);
    try testing.expectEqualStrings("/run/user/1000/squeezefs", socketDir(&buf, null, 1000, "/run/user/1000").?);
    try testing.expectEqualStrings("/tmp/squeezefs-il-1000", socketDir(&buf, null, 1000, null).?);
    try testing.expectEqualStrings("/tmp/squeezefs-il-1000", socketDir(&buf, null, 1000, "").?);
}

test "the trust predicates: a dir owned by root or the owner with no group/world write bit; a peer uid of root or the mount owner — and NOT our own uid" {
    try testing.expect(dirTrusted(0, 0o755, 1000));
    try testing.expect(dirTrusted(1000, 0o700, 1000));
    try testing.expect(!dirTrusted(1001, 0o700, 1000));
    try testing.expect(!dirTrusted(1000, 0o775, 1000));
    try testing.expect(!dirTrusted(0, 0o1777, 1000));
    try testing.expect(peerTrusted(0, 1000));
    try testing.expect(peerTrusted(1000, 1000));
    try testing.expect(!peerTrusted(1001, 1000));
    try testing.expect(!peerTrusted(1000, 0));
}

const good_facts: FdFacts = .{ .flags = .{ .accmode = .rdwr }, .stat = .{ .kind = .regular, .dev = 77, .ino = 1234, .nlink = 1 } };

test "THE FD SCREEN rule by rule: O_PATH, a non-regular fd, a foreign st_dev, append/sync/dsync/tmpfile/nlink 0, an invalid access mode — each its class; rights follow the DESCRIPTION's mode; O_DIRECT travels" {
    const ok = screenFd(good_facts, 77, false).ok;
    try testing.expectEqual(@as(u64, 1234), ok.ino);
    try testing.expect(ok.rights.read and ok.rights.write and !ok.rights.odirect);
    var f = good_facts;
    f.flags = null;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.flags.?.path = true;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.stat = null;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.stat.?.kind = .other;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f.stat.?.kind = .directory;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    // The SDK's HELLO credential may be the mount root directory.
    try testing.expect(screenFd(f, 77, true) == .ok);
    f = good_facts;
    f.stat.?.dev = 78;
    try testing.expectEqual(RefuseClass.mount, screenFd(f, 77, false).refuse);
    try testing.expectEqual(RefuseClass.mount, screenFd(good_facts, null, false).refuse);
    // O_PATH is judged BEFORE the device: a foreign O_PATH fd is `flags`.
    f.flags.?.path = true;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.flags.?.append = true;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.flags.?.sync = true;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.flags.?.dsync = true;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.flags.?.tmpfile = true;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.stat.?.nlink = 0;
    try testing.expectEqual(RefuseClass.flags, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.flags.?.accmode = .invalid;
    try testing.expectEqual(RefuseClass.rights, screenFd(f, 77, false).refuse);
    f = good_facts;
    f.flags.?.accmode = .rdonly;
    f.flags.?.direct = true;
    const ro = screenFd(f, 77, false).ok;
    try testing.expect(ro.rights.read and !ro.rights.write and ro.rights.odirect);
    f.flags.?.accmode = .wronly;
    const wo = screenFd(f, 77, false).ok;
    try testing.expect(!wo.rights.read and wo.rights.write);
}

fn policy() Policy {
    return .{ .armed = true, .our_commit = "0123456789ab", .arena_cap_bytes = 1 << 30, .per_uid_session_cap = 64 };
}

fn testHello(commit: []const u8, nonce: *const Nonce) wire.Hello {
    var h: wire.Hello = .{};
    wire.setField(&h.build_commit, commit) catch unreachable;
    h.nonce = nonce.*;
    return h;
}

test "the HELLO ladder in order: disabled → version (abi, commit, kind, SDK window) → dirty → nonce → peercred → no fd / the screen's class → budget (shed, cap, per-uid) → admit with the footprint" {
    const nonces = NonceState.init(&testFill, 0);
    const peer: PeerCred = .{ .pid = 1, .uid = 1000, .gid = 1000 };
    const h = testHello("0123456789ab", &nonces.current);
    var p = policy();
    // Disabled first, before any fd is looked at.
    p.armed = false;
    try testing.expectEqual(RefuseClass.disabled, admitHello(p, &h, null, null, null, &nonces).refuse);
    p.armed = true;
    // Version: abi, commit, kind, SDK window.
    var bad = h;
    bad.abi += 1;
    try testing.expectEqual(RefuseClass.version, admitHello(p, &bad, good_facts, 77, peer, &nonces).refuse);
    try testing.expectEqual(RefuseClass.version, admitHello(p, &testHello("0123456789ac", &nonces.current), good_facts, 77, peer, &nonces).refuse);
    bad = h;
    bad.client_kind = 7;
    try testing.expectEqual(RefuseClass.version, admitHello(p, &bad, good_facts, 77, peer, &nonces).refuse);
    var sdk = h;
    sdk.client_kind = 1;
    sdk.sdk_abi = wire.SDK_ABI_CURRENT + 1;
    try testing.expectEqual(RefuseClass.version, admitHello(p, &sdk, good_facts, 77, peer, &nonces).refuse);
    sdk.sdk_abi = wire.SDK_ABI_CURRENT;
    // The SDK pairs on the window, not the commit; its credential may be a directory.
    var dir_facts = good_facts;
    dir_facts.stat.?.kind = .directory;
    const sdk_ok = admitHello(p, &sdk, dir_facts, 77, peer, &nonces).admit;
    try testing.expectEqual(wire.ClientKind.sdk, sdk_ok.kind);
    // Dirty: refused unless the seam admits it (and then counted).
    var dp = p;
    dp.our_commit = "0123456789ab-dirty";
    try testing.expectEqual(RefuseClass.version, admitHello(dp, &testHello("0123456789ab-dirty", &nonces.current), good_facts, 77, peer, &nonces).refuse);
    dp.allow_dirty = true;
    try testing.expect(admitHello(dp, &testHello("0123456789ab-dirty", &nonces.current), good_facts, 77, peer, &nonces).admit.dev_override);
    try testing.expectEqual(RefuseClass.version, admitHello(dp, &testHello("unknown", &nonces.current), good_facts, 77, peer, &nonces).refuse);
    // Nonce.
    var stale: Nonce = @splat(9);
    try testing.expectEqual(RefuseClass.nonce, admitHello(p, &testHello("0123456789ab", &stale), good_facts, 77, peer, &nonces).refuse);
    stale = nonces.current;
    // Peercred unreadable.
    try testing.expectEqual(RefuseClass.internal, admitHello(p, &h, good_facts, 77, null, &nonces).refuse);
    // No fd; the screen's own classes.
    try testing.expectEqual(RefuseClass.flags, admitHello(p, &h, null, 77, peer, &nonces).refuse);
    var f = good_facts;
    f.stat.?.dev = 1;
    try testing.expectEqual(RefuseClass.mount, admitHello(p, &h, f, 77, peer, &nonces).refuse);
    f = good_facts;
    f.flags.?.accmode = .invalid;
    try testing.expectEqual(RefuseClass.rights, admitHello(p, &h, f, 77, peer, &nonces).refuse);
    // Budget: shed, cap, per-uid.
    var bp = p;
    bp.shed_target_bytes = 100;
    bp.arena_in_use_bytes = 200;
    try testing.expectEqual(RefuseClass.budget, admitHello(bp, &h, good_facts, 77, peer, &nonces).refuse);
    bp = p;
    bp.arena_cap_bytes = 64 * layout.MiB;
    try testing.expectEqual(RefuseClass.budget, admitHello(bp, &h, good_facts, 77, peer, &nonces).refuse);
    bp = p;
    bp.uid_sessions = 64;
    try testing.expectEqual(RefuseClass.budget, admitHello(bp, &h, good_facts, 77, peer, &nonces).refuse);
    // Admitted: the default geometry, its footprint, the credential's rights.
    const a = admitHello(p, &h, good_facts, 77, peer, &nonces).admit;
    try testing.expectEqual(wire.ClientKind.shim, a.kind);
    try testing.expect(a.geometry.eql(layout.Geometry.default));
    try testing.expectEqual((try layout.SessionLayout.compute(layout.Geometry.default)).total_bytes, a.footprint_bytes);
    try testing.expect(!a.dev_override);
    try testing.expectEqual(@as(u64, 1234), a.credential.ino);
}

test "the SDK's geometry request is clamped under the cap and degrades to the default below max_op; slots clamp to the ring" {
    var p = policy();
    p.arena_cap_bytes = 200 * layout.MiB;
    p.arena_in_use_bytes = 100 * layout.MiB;
    var h: wire.Hello = .{ .client_kind = 1, .sdk_abi = wire.SDK_ABI_CURRENT, .arena_bytes_req = 150 * layout.MiB + 100, .slots_req = 5000 };
    const g = grantGeometry(p, &h, .sdk);
    try testing.expectEqual(100 * layout.MiB, g.arena_bytes);
    try testing.expectEqual(@as(u32, 1024), g.slots);
    h.arena_bytes_req = 4096;
    h.slots_req = 2;
    const g2 = grantGeometry(p, &h, .sdk);
    try testing.expectEqual(layout.DEFAULT_ARENA_BYTES, g2.arena_bytes);
    try testing.expectEqual(@as(u32, 2), g2.slots);
    h.arena_bytes_req = 96 * layout.MiB + 5;
    try testing.expectEqual(96 * layout.MiB, grantGeometry(p, &h, .sdk).arena_bytes);
    // The shim never asks.
    try testing.expect(grantGeometry(p, &h, .shim).eql(layout.Geometry.default));
}

test "heartbeat watch: a moving word is alive; 20 slices unchanged asks for the probe; a move after resets the clock" {
    var w = HeartbeatWatch.init(100, 0);
    try testing.expectEqual(HeartbeatWatch.Step.alive, w.observe(100, WAIT_SLICE_NS));
    try testing.expectEqual(HeartbeatWatch.Step.alive, w.observe(100, HEARTBEAT_STALE_NS - 1));
    try testing.expectEqual(HeartbeatWatch.Step.probe, w.observe(100, HEARTBEAT_STALE_NS));
    try testing.expectEqual(HeartbeatWatch.Step.alive, w.observe(101, HEARTBEAT_STALE_NS + 1));
    try testing.expectEqual(HeartbeatWatch.Step.alive, w.observe(101, 2 * HEARTBEAT_STALE_NS));
    try testing.expectEqual(HeartbeatWatch.Step.probe, w.observe(101, 2 * HEARTBEAT_STALE_NS + 1));
    try testing.expectEqual(@as(u64, std.time.ns_per_s), HEARTBEAT_STALE_NS);
    try testing.expectEqual(@as(u64, 30 * std.time.ns_per_s), OP_TIMEOUT_NS);
}

test "knob faces: the dirty seam, the op-timeout seam and the socket dir read their rows" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expect(!allowDirtyFromEnv(&env));
    try testing.expectEqual(OP_TIMEOUT_NS, opTimeoutFromEnv(&env));
    var buf: DirBuf = undefined;
    try testing.expectEqualStrings("/tmp/squeezefs-il-1000", socketDirFromEnv(&buf, &env, 1000).?);
    try env.put("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", "1");
    try env.put("SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS", "250");
    try env.put("SQUEEZEFS_IPC_SOCKET_DIR", "none");
    try testing.expect(allowDirtyFromEnv(&env));
    try testing.expectEqual(250 * std.time.ns_per_ms, opTimeoutFromEnv(&env));
    try testing.expectEqual(@as(?[]const u8, null), socketDirFromEnv(&buf, &env, 1000));
}

test "the direct arm's device screen: root always; the owner by the owner bit, the group by the group bit, others by the other bit — never a bit that is not theirs" {
    try testing.expect(deviceReadable(0o600, 1000, 6, 0, 0));
    try testing.expect(deviceReadable(0o600, 1000, 6, 1000, 1000));
    try testing.expect(!deviceReadable(0o200, 1000, 6, 1000, 1000));
    try testing.expect(deviceReadable(0o660, 0, 6, 1000, 6));
    try testing.expect(!deviceReadable(0o620, 0, 6, 1000, 6));
    try testing.expect(!deviceReadable(0o660, 0, 6, 1000, 7));
    try testing.expect(deviceReadable(0o664, 0, 6, 1000, 7));
    try testing.expect(!deviceReadable(0o660, 1000, 6, 2000, 7));
    try testing.expect(deviceWritable(0o600, 1000, 6, 0, 0));
    try testing.expect(deviceWritable(0o600, 1000, 6, 1000, 1000));
    try testing.expect(!deviceWritable(0o400, 1000, 6, 1000, 1000));
    try testing.expect(deviceWritable(0o660, 0, 6, 1000, 6));
    try testing.expect(!deviceWritable(0o640, 0, 6, 1000, 6));
    try testing.expect(!deviceWritable(0o664, 0, 6, 1000, 7));
    try testing.expect(deviceWritable(0o666, 0, 6, 1000, 7));
}
