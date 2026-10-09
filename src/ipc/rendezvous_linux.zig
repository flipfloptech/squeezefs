//! The rendezvous on Linux (charter §7; spec `docs/porting/il_session.md`
//! §2): the `SOCK_SEQPACKET` listeners (abstract + path), the ctl datagram
//! send/receive with the `SCM_RIGHTS` discipline (VAL-5: own EVERY received
//! descriptor at once; refuse `MSG_CTRUNC` or more than one fd), the fd
//! facts THE SCREEN judges (`F_GETFL` + `statx`), `SO_PEERCRED`, the
//! daemon's one-connection HELLO acceptor, and the client's connect ladder
//! with its three authentication rungs IN ORDER:
//!
//!   1. the path rung's PARENT directory is owned by root or the mount
//!      owner and has no group/world write bit — else no `connect`;
//!   2. `SO_PEERCRED` after connect, BEFORE HELLO: the daemon's uid ∈
//!      {0, mount_uid} — else no HELLO, no `SCM_RIGHTS`;
//!   3. the received memfd carries all three seals — else nothing is mapped.
//!
//! Raw `std.os.linux` only (the shim links this; decision 0003 §F).

const std = @import("std");
const linux = std.os.linux;
/// Plain structs (no libc symbol behind them): the kernel's iovec shape.
const iovec = std.posix.iovec;
const iovec_const = std.posix.iovec_const;
const layout = @import("layout.zig");
const wire = @import("wire.zig");
const rendezvous = @import("rendezvous.zig");
const shm = @import("shm.zig");
const ipc = @import("ipc.zig");
const log = @import("../core/log.zig");

pub const Error = error{
    SocketFailed,
    BindFailed,
    ListenFailed,
    AcceptFailed,
    ConnectFailed,
    /// The peer closed (EOF / ECONNRESET) — the daemon is gone or the
    /// client died.
    Eof,
    SendFailed,
    /// A frame the protocol does not allow here: a truncated control
    /// message, more than one descriptor, a malformed datagram, an
    /// unexpected tag.
    Protocol,
    /// Rung 1: the path rendezvous directory is not trustworthy.
    UntrustedRendezvous,
    /// Rung 2: the daemon's uid is neither root nor the mount owner.
    PeerUntrusted,
    /// The directory a path listener would bind in is not trustworthy
    /// (the host degrades to abstract-only, loudly).
    UntrustedDir,
    /// `SQUEEZEFS_IPC_SOCKET_DIR`'s path is too long for `sun_path`.
    PathTooLong,
    /// The daemon answered REFUSE; the class travels beside.
    Refused,
};

pub const ucred = extern struct { pid: i32, uid: u32, gid: u32 };

/// The nonce's entropy: `getrandom(2)`, looping on a short or interrupted
/// read (the pool is initialised long before a daemon mounts).
pub fn fillNonce(n: *rendezvous.Nonce) void {
    var got: usize = 0;
    while (got < n.len) {
        const rc = linux.getrandom(n[got..].ptr, n.len - got, 0);
        switch (linux.errno(rc)) {
            .SUCCESS => got += rc,
            .INTR => continue,
            else => unreachable, // getrandom(buf, 32, 0) fails only on EFAULT
        }
    }
}

pub const MAX_FDS_PER_FRAME = 1;
const cmsg_hdr_len = std.mem.alignForward(usize, @sizeOf(linux.cmsghdr), linux.cmsg_align);
const cmsg_space_one_fd = cmsg_hdr_len + std.mem.alignForward(usize, @sizeOf(linux.fd_t), linux.cmsg_align);

/// A received datagram: its length and the ONE descriptor it may carry.
pub const Received = struct {
    len: usize,
    fd: ?linux.fd_t,
};

fn closeFd(fd: linux.fd_t) void {
    _ = linux.close(fd);
}

// ---------------------------------------------------------------------------
// Datagrams.
// ---------------------------------------------------------------------------

/// One `sendmsg(MSG_NOSIGNAL)`; `fd` rides `SCM_RIGHTS` (the kernel dups it
/// into the peer — the caller keeps its own).
pub fn sendCtl(sock: linux.fd_t, frame: []const u8, fd: ?linux.fd_t) Error!void {
    var iov = [_]iovec_const{.{ .base = frame.ptr, .len = frame.len }};
    var control: [cmsg_space_one_fd]u8 align(@alignOf(linux.cmsghdr)) = @splat(0);
    var msg: linux.msghdr_const = .{ .name = null, .namelen = 0, .iov = &iov, .iovlen = 1, .control = null, .controllen = 0, .flags = 0 };
    if (fd) |f| {
        const hdr: *linux.cmsghdr = @ptrCast(&control);
        hdr.* = .{ .len = cmsg_hdr_len + @sizeOf(linux.fd_t), .level = linux.SOL.SOCKET, .type = linux.SCM.RIGHTS };
        @memcpy(control[cmsg_hdr_len..][0..@sizeOf(linux.fd_t)], std.mem.asBytes(&f));
        msg.control = &control;
        msg.controllen = control.len;
    }
    while (true) {
        const rc = linux.sendmsg(sock, &msg, linux.MSG.NOSIGNAL);
        switch (linux.errno(rc)) {
            .SUCCESS => return if (rc == frame.len) {} else error.SendFailed,
            .INTR => continue,
            .PIPE, .CONNRESET, .NOTCONN => return error.Eof,
            else => return error.SendFailed,
        }
    }
}

/// One `recvmsg(MSG_CMSG_CLOEXEC)` into `buf`. Takes ownership of EVERY
/// descriptor the kernel installed (the count comes from `cmsg_len`, never
/// "one per cmsg"); `MSG_CTRUNC` or more than one fd ⇒ every received fd
/// is closed and `Protocol`; a zero-length receive ⇒ `Eof`.
pub fn recvCtl(sock: linux.fd_t, buf: []u8) Error!Received {
    var iov = [_]iovec{.{ .base = buf.ptr, .len = buf.len }};
    // Room for MORE than the one fd we accept, so an attacker's extra
    // descriptors are received (and closed) rather than left dangling.
    var control: [cmsg_hdr_len + 64 * @sizeOf(linux.fd_t)]u8 align(@alignOf(linux.cmsghdr)) = undefined;
    var msg: linux.msghdr = .{ .name = null, .namelen = 0, .iov = &iov, .iovlen = 1, .control = &control, .controllen = control.len, .flags = 0 };
    while (true) {
        const rc = linux.recvmsg(sock, &msg, linux.MSG.CMSG_CLOEXEC);
        switch (linux.errno(rc)) {
            .SUCCESS => {},
            .INTR => continue,
            .CONNRESET => return error.Eof,
            .AGAIN => return error.Eof,
            else => return error.Protocol,
        }
        var fds: [64]linux.fd_t = undefined;
        var n_fds: usize = 0;
        var off: usize = 0;
        while (off + @sizeOf(linux.cmsghdr) <= msg.controllen) {
            const hdr: *const linux.cmsghdr = @ptrCast(@alignCast(&control[off]));
            if (hdr.len < cmsg_hdr_len or off + hdr.len > msg.controllen) break;
            if (hdr.level == linux.SOL.SOCKET and hdr.type == linux.SCM.RIGHTS) {
                const count = (hdr.len - cmsg_hdr_len) / @sizeOf(linux.fd_t);
                var i: usize = 0;
                while (i < count and n_fds < fds.len) : (i += 1) {
                    @memcpy(std.mem.asBytes(&fds[n_fds]), control[off + cmsg_hdr_len + i * @sizeOf(linux.fd_t) ..][0..@sizeOf(linux.fd_t)]);
                    n_fds += 1;
                }
            }
            off += std.mem.alignForward(usize, hdr.len, linux.cmsg_align);
        }
        if (msg.flags & linux.MSG.CTRUNC != 0 or n_fds > MAX_FDS_PER_FRAME) {
            for (fds[0..n_fds]) |f| closeFd(f);
            return error.Protocol;
        }
        if (rc == 0 and n_fds == 0) return error.Eof;
        return .{ .len = rc, .fd = if (n_fds == 1) fds[0] else null };
    }
}

pub fn peerCred(sock: linux.fd_t) ?rendezvous.PeerCred {
    var cred: ucred = undefined;
    var len: linux.socklen_t = @sizeOf(ucred);
    if (linux.errno(linux.getsockopt(sock, linux.SOL.SOCKET, linux.SO.PEERCRED, @ptrCast(&cred), &len)) != .SUCCESS) return null;
    if (len != @sizeOf(ucred)) return null;
    return .{ .pid = @intCast(cred.pid), .uid = cred.uid, .gid = cred.gid };
}

pub fn setRecvTimeout(sock: linux.fd_t, ns: u64) void {
    const tv: linux.timeval = .{ .sec = @intCast(ns / std.time.ns_per_s), .usec = @intCast((ns % std.time.ns_per_s) / std.time.ns_per_us) };
    _ = linux.setsockopt(sock, linux.SOL.SOCKET, linux.SO.RCVTIMEO, @ptrCast(&tv), @sizeOf(linux.timeval));
}

/// The client's liveness probe of its ctl socket (the heartbeat ladder's
/// second rung): `recvmsg(MSG_PEEK | MSG_DONTWAIT)` — EOF or ECONNRESET ⇒
/// the daemon is gone; EAGAIN (or a pending datagram) ⇒ alive.
pub fn socketAlive(sock: linux.fd_t) bool {
    var byte: [1]u8 = undefined;
    var iov = [_]iovec{.{ .base = &byte, .len = 1 }};
    var msg: linux.msghdr = .{ .name = null, .namelen = 0, .iov = &iov, .iovlen = 1, .control = null, .controllen = 0, .flags = 0 };
    const rc = linux.recvmsg(sock, &msg, linux.MSG.PEEK | linux.MSG.DONTWAIT);
    return switch (linux.errno(rc)) {
        .SUCCESS => rc != 0,
        .AGAIN, .INTR => true,
        else => false,
    };
}

// ---------------------------------------------------------------------------
// The fd facts the screen judges.
// ---------------------------------------------------------------------------

fn makedev(major: u32, minor: u32) u64 {
    return (@as(u64, major) << 8) | minor | (@as(u64, major & ~@as(u32, 0xfff)) << 32) | (@as(u64, minor & ~@as(u32, 0xff)) << 12);
}

pub fn fdFacts(fd: linux.fd_t) rendezvous.FdFacts {
    var facts: rendezvous.FdFacts = .{ .flags = null, .stat = null };
    const fl_rc = linux.fcntl(fd, linux.F.GETFL, 0);
    if (linux.errno(fl_rc) == .SUCCESS) {
        const o: linux.O = @bitCast(@as(u32, @truncate(fl_rc)));
        facts.flags = .{
            .accmode = @fromBackingInt(@intCast(@backingInt(o.ACCMODE))),
            .path = o.PATH,
            .append = o.APPEND,
            .sync = o.SYNC,
            .dsync = o.DSYNC,
            .tmpfile = o.TMPFILE,
            .direct = o.DIRECT,
        };
    }
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true, .NLINK = true, .INO = true }, &st)) == .SUCCESS) {
        const mode: u32 = st.mode;
        facts.stat = .{
            .kind = if (linux.S.ISREG(mode)) .regular else if (linux.S.ISDIR(mode)) .directory else .other,
            .dev = makedev(st.dev_major, st.dev_minor),
            .ino = st.ino,
            .nlink = st.nlink,
        };
    }
    return facts;
}

/// The device of the filesystem behind `fd` (the mount root's, resolved
/// before the host arms — decision 0002 ledger Q17).
pub fn deviceOf(fd: linux.fd_t) ?u64 {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true }, &st)) != .SUCCESS) return null;
    return makedev(st.dev_major, st.dev_minor);
}

/// The owner and mode of the directory behind `dirfd` (both trust screens).
pub fn dirOwnerMode(dirfd: linux.fd_t) ?struct { uid: u32, mode: u32 } {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(dirfd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true, .MODE = true, .UID = true }, &st)) != .SUCCESS) return null;
    if (!linux.S.ISDIR(@as(u32, st.mode))) return null;
    return .{ .uid = st.uid, .mode = st.mode };
}

// ---------------------------------------------------------------------------
// Sockets.
// ---------------------------------------------------------------------------

fn seqpacket() Error!linux.fd_t {
    const rc = linux.socket(linux.AF.UNIX, linux.SOCK.SEQPACKET | linux.SOCK.CLOEXEC, 0);
    if (linux.errno(rc) != .SUCCESS) return error.SocketFailed;
    return @intCast(rc);
}

fn abstractAddr(name: []const u8, addr: *linux.sockaddr.un) Error!linux.socklen_t {
    if (name.len + 1 > addr.path.len) return error.PathTooLong;
    addr.* = .{ .path = @splat(0) };
    @memcpy(addr.path[1 .. 1 + name.len], name);
    return @intCast(@sizeOf(linux.sa_family_t) + 1 + name.len);
}

fn pathAddr(path: []const u8, addr: *linux.sockaddr.un) Error!linux.socklen_t {
    if (path.len >= addr.path.len) return error.PathTooLong;
    addr.* = .{ .path = @splat(0) };
    @memcpy(addr.path[0..path.len], path);
    return @intCast(@sizeOf(linux.sockaddr.un));
}

/// The abstract listener: zero residue, no permission bits, per network
/// namespace.
pub fn listenAbstract(name: []const u8, backlog: u32) Error!linux.fd_t {
    const fd = try seqpacket();
    errdefer closeFd(fd);
    var addr: linux.sockaddr.un = undefined;
    const len = try abstractAddr(name, &addr);
    if (linux.errno(linux.bind(fd, @ptrCast(&addr), len)) != .SUCCESS) return error.BindFailed;
    if (linux.errno(linux.listen(fd, backlog)) != .SUCCESS) return error.ListenFailed;
    return fd;
}

pub const PathListener = struct {
    fd: linux.fd_t,
    /// The advertised `<dir>/<name>.sock`.
    path_buf: [wire.SOCKET_NAME_LEN]u8,
    path_len: usize,

    pub fn path(self: *const PathListener) []const u8 {
        return self.path_buf[0..self.path_len];
    }

    /// Unlinks the socket file (shutdown) and closes.
    pub fn close(self: *PathListener) void {
        var z: [wire.SOCKET_NAME_LEN + 1]u8 = undefined;
        @memcpy(z[0..self.path_len], self.path());
        z[self.path_len] = 0;
        _ = linux.unlinkat(linux.AT.FDCWD, z[0..self.path_len :0], 0);
        closeFd(self.fd);
    }
};

/// The path listener: create the dir 0755; open it `O_DIRECTORY|O_NOFOLLOW|
/// O_CLOEXEC`; `fstat` the DIRFD: owned by our euid (or root) with no
/// group/world write bit — else `UntrustedDir` (the host logs and degrades
/// to abstract-only, never fails the mount); unlink/bind/chmod through
/// `/proc/self/fd/<dirfd>/<name>` (AF_UNIX has no `bindat`; a swapped
/// directory cannot redirect an open fd). The file is 0666 because
/// connecting is not a credential. Stale same-name residue is OUR corpse.
pub fn listenPath(dir: []const u8, name: []const u8, backlog: u32) Error!PathListener {
    var dir_z: [std.fs.max_path_bytes]u8 = undefined;
    if (dir.len >= dir_z.len) return error.PathTooLong;
    @memcpy(dir_z[0..dir.len], dir);
    dir_z[dir.len] = 0;
    const dz = dir_z[0..dir.len :0];
    _ = linux.mkdirat(linux.AT.FDCWD, dz, 0o755);
    const drc = linux.openat(linux.AT.FDCWD, dz, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
    if (linux.errno(drc) != .SUCCESS) return error.UntrustedDir;
    const dirfd: linux.fd_t = @intCast(drc);
    defer closeFd(dirfd);
    const om = dirOwnerMode(dirfd) orelse return error.UntrustedDir;
    if (!rendezvous.dirTrusted(om.uid, om.mode, linux.geteuid())) return error.UntrustedDir;

    var self: PathListener = .{ .fd = -1, .path_buf = undefined, .path_len = 0 };
    const adv = std.fmt.bufPrint(&self.path_buf, "{s}/{s}.sock", .{ dir, name }) catch return error.PathTooLong;
    if (adv.len >= wire.SOCKET_NAME_LEN) return error.PathTooLong;
    self.path_len = adv.len;
    var name_z_buf: [wire.SOCKET_NAME_LEN]u8 = undefined;
    const name_z = std.fmt.bufPrintSentinel(&name_z_buf, "{s}.sock", .{name}, 0) catch return error.PathTooLong;
    var proc_buf: [128]u8 = undefined;
    const proc_path = std.fmt.bufPrintSentinel(&proc_buf, "/proc/self/fd/{d}/{s}", .{ dirfd, name_z }, 0) catch return error.PathTooLong;
    // unlink and chmod are dirfd-relative (no magic link needed); bind has
    // no `bindat`, so it goes through the dirfd's /proc path.
    _ = linux.unlinkat(dirfd, name_z, 0);
    const fd = try seqpacket();
    errdefer closeFd(fd);
    var addr: linux.sockaddr.un = undefined;
    const len = try pathAddr(proc_path, &addr);
    if (linux.errno(linux.bind(fd, @ptrCast(&addr), len)) != .SUCCESS) return error.BindFailed;
    const chmod_rc = linux.fchmodat(dirfd, name_z, 0o666);
    if (linux.errno(chmod_rc) != .SUCCESS) {
        log.err("ipc: chmod 0666 of the path socket failed ({t})", .{linux.errno(chmod_rc)});
        return error.BindFailed;
    }
    if (linux.errno(linux.listen(fd, backlog)) != .SUCCESS) return error.ListenFailed;
    self.fd = fd;
    return self;
}

pub fn accept(listener: linux.fd_t) Error!linux.fd_t {
    while (true) {
        const rc = linux.accept4(listener, null, null, linux.SOCK.CLOEXEC);
        switch (linux.errno(rc)) {
            .SUCCESS => return @intCast(rc),
            .INTR => continue,
            else => return error.AcceptFailed,
        }
    }
}

pub fn connectAbstract(name: []const u8) Error!linux.fd_t {
    const fd = try seqpacket();
    errdefer closeFd(fd);
    var addr: linux.sockaddr.un = undefined;
    const len = try abstractAddr(name, &addr);
    if (linux.errno(linux.connect(fd, &addr, len)) != .SUCCESS) return error.ConnectFailed;
    return fd;
}

pub fn connectPath(path: []const u8) Error!linux.fd_t {
    const fd = try seqpacket();
    errdefer closeFd(fd);
    var addr: linux.sockaddr.un = undefined;
    const len = try pathAddr(path, &addr);
    if (linux.errno(linux.connect(fd, &addr, len)) != .SUCCESS) return error.ConnectFailed;
    return fd;
}

/// Rung 1: the advertised socket's PARENT directory, opened `O_PATH|
/// O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`, must be owned by root or the mount
/// owner with no group/world write bit. The abstract rung needs no screen.
pub fn pathRungTrusted(socket_path: []const u8, mount_uid: u32) bool {
    const parent = std.fs.path.dirname(socket_path) orelse return false;
    var z: [std.fs.max_path_bytes]u8 = undefined;
    if (parent.len >= z.len) return false;
    @memcpy(z[0..parent.len], parent);
    z[parent.len] = 0;
    const rc = linux.openat(linux.AT.FDCWD, z[0..parent.len :0], .{ .ACCMODE = .RDONLY, .PATH = true, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return false;
    const dirfd: linux.fd_t = @intCast(rc);
    defer closeFd(dirfd);
    const om = dirOwnerMode(dirfd) orelse return false;
    return rendezvous.dirTrusted(om.uid, om.mode, mount_uid);
}

/// The client's connect ladder: abstract first; then the path rung, only
/// after rung 1 passes. Returns the connected ctl socket with the receive
/// timeout set.
pub fn connectLadder(blob: *const wire.Il0Blob, mount_uid: u32) Error!linux.fd_t {
    const sock = connectAbstract(blob.socketName()) catch |abstract_err| blk: {
        const p = blob.socketPath();
        if (p.len == 0) return abstract_err;
        if (!pathRungTrusted(p, mount_uid)) return error.UntrustedRendezvous;
        break :blk try connectPath(p);
    };
    setRecvTimeout(sock, rendezvous.CTL_RECV_TIMEOUT_NS);
    return sock;
}

/// Rung 2, BEFORE the credential fd leaves the process.
pub fn authenticatePeer(sock: linux.fd_t, mount_uid: u32) Error!void {
    const cred = peerCred(sock) orelse return error.PeerUntrusted;
    if (!rendezvous.peerTrusted(cred.uid, mount_uid)) return error.PeerUntrusted;
}

// ---------------------------------------------------------------------------
// The HELLO exchange.
// ---------------------------------------------------------------------------

pub const HelloRequest = struct {
    kind: wire.ClientKind = .shim,
    build_commit: []const u8,
    nonce: rendezvous.Nonce,
    sdk_abi: u32 = 0,
    arena_bytes_req: u64 = 0,
    slots_req: u32 = 0,
};

pub const HelloReply = union(enum) {
    /// The sealed memfd (rung 3 NOT yet checked: the caller runs
    /// `shm.checkSeals` before mapping).
    session: struct { memfd: linux.fd_t, features: u64 },
    refused: struct { class: wire.RefuseClass, sdk_abi_min: u32, sdk_abi_max: u32 },
};

/// The client's half: send HELLO with the credential fd, receive the reply
/// (one fd at most; a session reply carrying none is `Protocol`).
pub fn hello(sock: linux.fd_t, cred_fd: linux.fd_t, req: HelloRequest) Error!HelloReply {
    var h: wire.Hello = .{ .client_kind = @backingInt(req.kind), .sdk_abi = req.sdk_abi, .arena_bytes_req = req.arena_bytes_req, .slots_req = req.slots_req };
    wire.setField(&h.build_commit, req.build_commit) catch return error.Protocol;
    h.nonce = req.nonce;
    try sendCtl(sock, wire.frameBytes(&h), cred_fd);
    var buf: [wire.CTL_MSG_MAX]u8 = undefined;
    const r = try recvCtl(sock, &buf);
    const frame = buf[0..r.len];
    const tag = wire.tagOf(frame) catch {
        if (r.fd) |f| closeFd(f);
        return error.Protocol;
    };
    switch (tag) {
        .session_ok => {
            const ok = wire.decodeFixed(wire.SessionOk, frame) catch {
                if (r.fd) |f| closeFd(f);
                return error.Protocol;
            };
            const memfd = r.fd orelse return error.Protocol;
            return .{ .session = .{ .memfd = memfd, .features = ok.features } };
        },
        .refuse => {
            if (r.fd) |f| closeFd(f);
            const ref = wire.decodeFixed(wire.Refuse, frame) catch return error.Protocol;
            return .{ .refused = .{ .class = ref.refuseClass() catch return error.Protocol, .sdk_abi_min = ref.sdk_abi_min, .sdk_abi_max = ref.sdk_abi_max } };
        },
        else => {
            if (r.fd) |f| closeFd(f);
            return error.Protocol;
        },
    }
}

/// The daemon's half over ONE accepted connection: the first datagram must
/// be HELLO (anything else drops the connection); the credential's facts are
/// read and the dup closed at once (the daemon needs `(st_dev, st_ino,
/// flags, mode)`, never a live handle); the pure ladder decides; an
/// admission builds the sealed memfd, sends SESSION_OK with it and closes
/// the daemon's fd (the mapping keeps the memory); a refusal is counted,
/// logged once per class and answered REFUSE.
pub const Accepted = struct {
    admission: rendezvous.Admission,
    peer: rendezvous.PeerCred,
    mapped: shm.Mapped,
    layout: layout.SessionLayout,
};

pub const AcceptOutcome = union(enum) {
    accepted: Accepted,
    refused: wire.RefuseClass,
    /// Not a HELLO, or a malformed one: the connection is dropped.
    dropped,
};

pub fn acceptHello(sock: linux.fd_t, policy: rendezvous.Policy, nonces: *const rendezvous.NonceState, mount_dev: ?u64, thp: bool) AcceptOutcome {
    var buf: [wire.CTL_MSG_MAX]u8 = undefined;
    const r = recvCtl(sock, &buf) catch return .dropped;
    defer if (r.fd) |f| {
        closeFd(f);
    };
    const frame = buf[0..r.len];
    const h = wire.decodeFixed(wire.Hello, frame) catch return .dropped;
    const peer = peerCred(sock);
    const facts: ?rendezvous.FdFacts = if (r.fd) |f| fdFacts(f) else null;
    switch (rendezvous.admitHello(policy, &h, facts, mount_dev, peer, nonces)) {
        .refuse => |class| {
            ipc.countRefusal(class);
            const ref: wire.Refuse = .{ .class = @backingInt(class), .sdk_abi_min = if (class == .version) wire.sdk_abi_accepted[wire.sdk_abi_accepted.len - 1] else 0, .sdk_abi_max = if (class == .version) wire.sdk_abi_accepted[0] else 0 };
            sendCtl(sock, wire.frameBytes(&ref), null) catch {};
            return .{ .refused = class };
        },
        .admit => |adm| {
            if (adm.dev_override) ipc.stats.ipc_binds_dev_override.inc();
            const l = layout.SessionLayout.compute(adm.geometry) catch {
                ipc.countRefusal(.internal);
                return .{ .refused = .internal };
            };
            const created = shm.createSealed(l, thp) catch |e| {
                log.err("ipc: session memfd creation failed ({t}); refusing Internal", .{e});
                ipc.countRefusal(.internal);
                const ref: wire.Refuse = .{ .class = @backingInt(wire.RefuseClass.internal) };
                sendCtl(sock, wire.frameBytes(&ref), null) catch {};
                return .{ .refused = .internal };
            };
            defer closeFd(created.fd);
            const ok: wire.SessionOk = .{};
            sendCtl(sock, wire.frameBytes(&ok), created.fd) catch {
                shm.unmap(created.mapped);
                return .dropped;
            };
            return .{ .accepted = .{ .admission = adm, .peer = peer.?, .mapped = created.mapped, .layout = l } };
        },
    }
}

// ---------------------------------------------------------------------------
// Tests: the datagram discipline and the sockets; the full ladder end to
// end lives in `client.zig` (the one-op protocol needs the client).
// ---------------------------------------------------------------------------

const testing = std.testing;

fn pair() ![2]linux.fd_t {
    var fds: [2]linux.fd_t = undefined;
    if (linux.errno(linux.socketpair(linux.AF.UNIX, linux.SOCK.SEQPACKET | linux.SOCK.CLOEXEC, 0, &fds)) != .SUCCESS) return error.SocketpairFailed;
    return fds;
}

fn testMemfd(name: [*:0]const u8) !linux.fd_t {
    const rc = linux.memfd_create(name, linux.MFD.CLOEXEC);
    if (linux.errno(rc) != .SUCCESS) return error.MemfdFailed;
    return @intCast(rc);
}

fn openFdCount() !usize {
    const rc = linux.openat(linux.AT.FDCWD, "/proc/self/fd", .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return error.ProcFailed;
    const fd: linux.fd_t = @intCast(rc);
    defer closeFd(fd);
    var buf: [8192]u8 = undefined;
    var count: usize = 0;
    while (true) {
        const n = linux.getdents64(fd, &buf, buf.len);
        if (linux.errno(n) != .SUCCESS or n == 0) break;
        var off: usize = 0;
        while (off < n) {
            const reclen = std.mem.readInt(u16, buf[off + 16 ..][0..2], .little);
            if (buf[off + 19] != '.') count += 1;
            off += reclen;
        }
    }
    // Minus the directory fd itself.
    return count - 1;
}

test "SCM_RIGHTS discipline: one fd rides a frame and is owned by the receiver; a frame with TWO fds is refused with both closed; a plain frame carries none; EOF is Eof" {
    const fds = try pair();
    defer closeFd(fds[0]);
    const before = try openFdCount();
    const m = try testMemfd("sqz-cred");
    const bind: wire.Bind = .{};
    try sendCtl(fds[0], wire.frameBytes(&bind), m);
    var buf: [256]u8 = undefined;
    const r = try recvCtl(fds[1], &buf);
    try testing.expectEqual(@as(usize, 8), r.len);
    try testing.expect(r.fd != null);
    try testing.expect(r.fd.? != m);
    closeFd(r.fd.?);
    // Two fds in one cmsg: refused, nothing leaks.
    var iov = [_]iovec_const{.{ .base = wire.frameBytes(&bind).ptr, .len = 8 }};
    var control: [cmsg_hdr_len + 2 * @sizeOf(linux.fd_t)]u8 align(@alignOf(linux.cmsghdr)) = @splat(0);
    const hdr: *linux.cmsghdr = @ptrCast(&control);
    hdr.* = .{ .len = cmsg_hdr_len + 2 * @sizeOf(linux.fd_t), .level = linux.SOL.SOCKET, .type = linux.SCM.RIGHTS };
    const two = [2]linux.fd_t{ m, m };
    @memcpy(control[cmsg_hdr_len..][0..8], std.mem.asBytes(&two));
    const msg: linux.msghdr_const = .{ .name = null, .namelen = 0, .iov = &iov, .iovlen = 1, .control = &control, .controllen = control.len, .flags = 0 };
    try testing.expectEqual(@as(usize, 8), linux.sendmsg(fds[0], &msg, linux.MSG.NOSIGNAL));
    try testing.expectError(error.Protocol, recvCtl(fds[1], &buf));
    closeFd(m);
    try testing.expectEqual(before, try openFdCount());
    // A plain frame: no fd.
    const unbind: wire.Unbind = .{ .binding_id = 4 };
    try sendCtl(fds[0], wire.frameBytes(&unbind), null);
    const r2 = try recvCtl(fds[1], &buf);
    try testing.expectEqual(@as(?linux.fd_t, null), r2.fd);
    try testing.expectEqual(@as(u64, 4), (try wire.decodeFixed(wire.Unbind, buf[0..r2.len])).binding_id);
    // Peer credentials are ours; the liveness probe says alive, then dead.
    const cred = peerCred(fds[1]).?;
    try testing.expectEqual(linux.geteuid(), cred.uid);
    try testing.expect(socketAlive(fds[1]));
    closeFd(fds[1]);
    try testing.expectError(error.Eof, recvCtl(fds[0], &buf));
    try testing.expect(!socketAlive(fds[0]));
}

test "fd facts: a regular O_RDWR|O_DIRECT file, an O_PATH fd, a directory, a memfd on a foreign device, an O_APPEND fd — the screen's inputs read off the DESCRIPTION" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const io = std.testing.io;
    var f = try tmp.dir.createFile(io, "f", .{ .read = true });
    f.close(io);
    var pbuf: [std.fs.max_path_bytes]u8 = undefined;
    const dir_path = pbuf[0..try tmp.dir.realPath(io, &pbuf)];
    var z: [std.fs.max_path_bytes]u8 = undefined;
    const file_z = try std.fmt.bufPrintSentinel(&z, "{s}/f", .{dir_path}, 0);
    const rw: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, file_z, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0));
    defer closeFd(rw);
    const facts = fdFacts(rw);
    try testing.expectEqual(rendezvous.AccessMode.rdwr, facts.flags.?.accmode);
    try testing.expect(!facts.flags.?.path and !facts.flags.?.append);
    try testing.expectEqual(rendezvous.FileKind.regular, facts.stat.?.kind);
    try testing.expectEqual(@as(u32, 1), facts.stat.?.nlink);
    const dev = deviceOf(rw).?;
    try testing.expectEqual(dev, facts.stat.?.dev);
    try testing.expect(rendezvous.screenFd(facts, dev, false) == .ok);
    const op: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, file_z, .{ .ACCMODE = .RDONLY, .PATH = true, .CLOEXEC = true }, 0));
    defer closeFd(op);
    try testing.expect(fdFacts(op).flags.?.path);
    try testing.expectEqual(wire.RefuseClass.flags, rendezvous.screenFd(fdFacts(op), dev, false).refuse);
    var dz: [std.fs.max_path_bytes]u8 = undefined;
    const dir_z = try std.fmt.bufPrintSentinel(&dz, "{s}", .{dir_path}, 0);
    const dfd: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, dir_z, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0));
    defer closeFd(dfd);
    try testing.expectEqual(rendezvous.FileKind.directory, fdFacts(dfd).stat.?.kind);
    try testing.expectEqual(wire.RefuseClass.flags, rendezvous.screenFd(fdFacts(dfd), dev, false).refuse);
    try testing.expect(rendezvous.screenFd(fdFacts(dfd), dev, true) == .ok);
    const m = try testMemfd("sqz-foreign");
    defer closeFd(m);
    const mf = fdFacts(m);
    try testing.expectEqual(rendezvous.FileKind.regular, mf.stat.?.kind);
    try testing.expect(mf.stat.?.dev != dev);
    try testing.expectEqual(wire.RefuseClass.mount, rendezvous.screenFd(mf, dev, false).refuse);
    const ap: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, file_z, .{ .ACCMODE = .WRONLY, .APPEND = true, .CLOEXEC = true }, 0));
    defer closeFd(ap);
    try testing.expect(fdFacts(ap).flags.?.append);
    try testing.expectEqual(wire.RefuseClass.flags, rendezvous.screenFd(fdFacts(ap), dev, false).refuse);
    const om = dirOwnerMode(dfd).?;
    try testing.expectEqual(linux.geteuid(), om.uid);
}

/// A scratch directory on a LOCAL filesystem: the repo's tmp dir is virtiofs
/// on the venue, which refuses `chmod` on a socket inode (EINVAL).
const LocalScratch = struct {
    buf: [64]u8 = undefined,
    len: usize = 0,

    fn init(self: *LocalScratch) ![]const u8 {
        const p = try std.fmt.bufPrintSentinel(&self.buf, "/tmp/sqz-ipc-test-{d}", .{linux.getpid()}, 0);
        self.len = p.len;
        _ = linux.mkdirat(linux.AT.FDCWD, p, 0o700);
        return p;
    }

    fn path(self: *const LocalScratch) []const u8 {
        return self.buf[0..self.len];
    }

    fn cleanup(self: *LocalScratch) void {
        var z: [64]u8 = undefined;
        for ([_][]const u8{ "private", "world" }) |sub| {
            const sp = std.fmt.bufPrintSentinel(&z, "{s}/{s}", .{ self.path(), sub }, 0) catch continue;
            _ = linux.unlinkat(linux.AT.FDCWD, sp, linux.AT.REMOVEDIR);
        }
        const root = std.fmt.bufPrintSentinel(&z, "{s}", .{self.path()}, 0) catch return;
        _ = linux.unlinkat(linux.AT.FDCWD, root, linux.AT.REMOVEDIR);
    }
};

test "listeners: an abstract SEQPACKET listener accepts a connect; a path listener in an owner-private dir binds, advertises, accepts and is unlinked at close; a world-writable dir is refused on both sides" {
    var scratch: LocalScratch = .{};
    const dir_path = try scratch.init();
    defer scratch.cleanup();
    var name_buf: [rendezvous.SOCKET_NAME_MAX]u8 = undefined;
    const name = rendezvous.socketName(&name_buf, @intCast(linux.getpid()), 0x1234abcd);
    const al = try listenAbstract(name, 64);
    defer closeFd(al);
    const c = try connectAbstract(name);
    defer closeFd(c);
    const a = try accept(al);
    defer closeFd(a);
    const ping: wire.AdminOk = .{};
    try sendCtl(c, wire.frameBytes(&ping), null);
    var buf: [64]u8 = undefined;
    try testing.expectEqual(@as(usize, 8), (try recvCtl(a, &buf)).len);
    // The path rung in a private dir.
    var sub_buf: [std.fs.max_path_bytes]u8 = undefined;
    const sub = try std.fmt.bufPrint(&sub_buf, "{s}/private", .{dir_path});
    var pl = try listenPath(sub, name, 64);
    try testing.expect(std.mem.endsWith(u8, pl.path(), ".sock"));
    try testing.expect(pathRungTrusted(pl.path(), linux.geteuid()));
    // A root-owned dir is trusted for every mount owner; a user-owned one
    // only for that user.
    try testing.expectEqual(linux.geteuid() == 0, pathRungTrusted(pl.path(), linux.geteuid() + 1));
    const pc = try connectPath(pl.path());
    defer closeFd(pc);
    const pa = try accept(pl.fd);
    defer closeFd(pa);
    try sendCtl(pc, wire.frameBytes(&ping), null);
    try testing.expectEqual(@as(usize, 8), (try recvCtl(pa, &buf)).len);
    var z: [std.fs.max_path_bytes]u8 = undefined;
    const sock_z = try std.fmt.bufPrintSentinel(&z, "{s}", .{pl.path()}, 0);
    var st: linux.Statx = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, sock_z, 0, .{ .MODE = true }, &st)));
    try testing.expectEqual(@as(u32, 0o666), @as(u32, st.mode) & 0o777);
    pl.close();
    try testing.expect(linux.errno(linux.statx(linux.AT.FDCWD, sock_z, 0, .{ .MODE = true }, &st)) != .SUCCESS);
    // A world-writable dir: the daemon refuses to bind there, the client refuses to connect there.
    const ww = try std.fmt.bufPrint(&sub_buf, "{s}/world", .{dir_path});
    var wz: [std.fs.max_path_bytes]u8 = undefined;
    const ww_z = try std.fmt.bufPrintSentinel(&wz, "{s}", .{ww}, 0);
    _ = linux.mkdirat(linux.AT.FDCWD, ww_z, 0o777);
    _ = linux.fchmodat(linux.AT.FDCWD, ww_z, 0o777);
    try testing.expectError(error.UntrustedDir, listenPath(ww, name, 64));
    var fake: [std.fs.max_path_bytes]u8 = undefined;
    try testing.expect(!pathRungTrusted(try std.fmt.bufPrint(&fake, "{s}/x.sock", .{ww}), linux.geteuid()));
}
