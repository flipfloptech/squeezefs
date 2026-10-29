//! The shim end to end IN PROCESS (linked mode): the ladder with the
//! mount's answers injected (a regular file on /tmp stands in for the
//! mount; the FUSE magic and the `il0` blob come from the probes), a real
//! `Session.establish` over a real SEQPACKET socket, SCM_RIGHTS and a
//! sealed memfd against the client library's `StubHost` (reads serve the
//! `offset + i` pattern, writes are acked), then every interposer BODY: positional, offsetful with the
//! mirror, vector, lseek arithmetic, dup refcounts, fcntl/mmap unbinds,
//! the hygiene sweep, the fork flush, and the daemon dying mid-op ⇒ the
//! real call. The engagement law: the host's `served` accounts for every
//! ring op and the shim's ledger for every passthrough.
//!
//! The `.so` itself (LD_PRELOAD on dd/cat/cp, a C program, the libaio
//! harness) is the venue smoke's (`smoke.zig`).

const std = @import("std");
const linux = std.os.linux;
const Atomic = std.atomic.Value;

const sys = @import("sys.zig");
const fd_table = @import("fd_table.zig");
const classify = @import("classify.zig");
const ops = @import("ops.zig");
const aio = @import("aio.zig");
const interpose = @import("interpose.zig");
const lifecycle = @import("lifecycle.zig");
const client = @import("../ipc/client.zig");
const wire = @import("../ipc/wire.zig");
const layout = @import("../ipc/layout.zig");
const rendezvous = @import("../ipc/rendezvous.zig");
const rl = @import("../ipc/rendezvous_linux.zig");
const shm = @import("../ipc/shm.zig");
const clock = @import("../core/clock.zig");

const testing = std.testing;

/// The aio reap loop's wedge bound: four stub completions land in
/// microseconds and the reap re-checks its doorbell park every 5 ms, so a
/// completion still missing after this is LOST — the row then prints the
/// host's account beside the reaped count to say on which side.
const REAP_BOUND_NS: u64 = 5 * std.time.ns_per_s;

/// The injected mount: real `fstat`/`F_GETFL`/`lseek` on the scratch file,
/// FUSE magic and the host's blob from the fake.
const Mount = struct {
    var blob_bytes: ?[]const u8 = null;

    fn fsType(_: sys.fd_t) ?i64 {
        return sys.FUSE_SUPER_MAGIC;
    }
    fn readBlob(_: sys.fd_t, buf: []u8) ?usize {
        const b = blob_bytes orelse return null;
        @memcpy(buf[0..b.len], b);
        return b.len;
    }
    const probes: classify.Probes = .{ .fstat = sys.fstat, .fsType = fsType, .getfl = sys.getfl, .readBlob = readBlob, .tell = sys.tell };
};

const Rig = struct {
    host: client.StubHost,
    st: *classify.State,
    path_buf: [64]u8 = undefined,
    path_len: usize = 0,
    blob: wire.Il0Blob = undefined,
    acceptor: ?std.Thread = null,

    fn init(self: *Rig) !void {
        self.* = .{ .host = undefined, .st = undefined };
        self.st = try testing.allocator.create(classify.State);
        errdefer testing.allocator.destroy(self.st);
        try classify.initState(self.st, testing.allocator, sys.mapAnon);
        errdefer self.st.env.deinit();
        // ONE shard: the stub host accepts a single session.
        self.st.sessions_per_mount = 1;
        const p = try std.fmt.bufPrintSentinel(&self.path_buf, "/tmp/sqz-il-e2e-{d}", .{linux.getpid()}, 0);
        self.path_len = p.len;
        const fd = try self.open(sys.O.RDWR | sys.O.CREAT);
        defer sys.close(fd);
        _ = linux.ftruncate(fd, 1 << 20);
        const dev = sys.fstat(fd).?.dev;
        try self.host.init(.{ .armed = true, .our_commit = lifecycle.build_commit, .arena_cap_bytes = 1 << 30, .per_uid_session_cap = 64, .default_geometry = .{ .ring_entries = 64, .slots = 64, .arena_bytes = 64 * 16 * layout.PAGE_BYTES, .max_op_bytes = 4 * layout.PAGE_BYTES } }, dev);
        self.blob = try self.host.blob(lifecycle.build_commit);
        Mount.blob_bytes = self.blob.bytes();
        self.accept();
    }

    fn accept(self: *Rig) void {
        const Acceptor = struct {
            fn run(h: *client.StubHost) void {
                h.acceptOne() catch {};
            }
        };
        self.acceptor = std.Thread.spawn(.{}, Acceptor.run, .{&self.host}) catch null;
    }

    fn joinAcceptor(self: *Rig) void {
        if (self.acceptor) |t| t.join();
        self.acceptor = null;
    }

    fn deinit(self: *Rig) void {
        self.joinAcceptor();
        self.host.deinit();
        _ = linux.unlinkat(linux.AT.FDCWD, self.path(), 0);
        self.st.env.deinit();
        testing.allocator.destroy(self.st);
        Mount.blob_bytes = null;
    }

    fn path(self: *Rig) [:0]const u8 {
        return self.path_buf[0..self.path_len :0];
    }

    fn open(self: *Rig, flags: u32) !sys.fd_t {
        const rc = linux.openat(linux.AT.FDCWD, self.path(), @bitCast(flags | sys.O.CLOEXEC), 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
        return @intCast(rc);
    }

    /// `open` + the ladder, as the `open` wrapper does it.
    fn openBound(self: *Rig, flags: u32) !sys.fd_t {
        const epoch = self.st.table.currentEpoch();
        const fd = try self.open(flags);
        if (classify.classifyAndBind(self.st, Mount.probes, fd, epoch) != .bound) return error.NotBound;
        self.joinAcceptor();
        return fd;
    }

    fn served(self: *Rig) u64 {
        return self.host.served.load(.monotonic);
    }
};

/// The stub's READ answer: byte `i` of a read at `offset` is `offset + i`.
fn expectPattern(buf: []const u8, offset: u64) !void {
    for (buf, 0..) |b, i| try testing.expectEqual(@as(u8, @truncate(offset + i)), b);
}

fn expectDone(v: ops.Verdict(isize), want: isize) !void {
    switch (v) {
        .done => |n| try testing.expectEqual(want, n),
        .real => return error.TestExpectedServed,
    }
}

test "e2e: the ladder establishes one session per (mount, shard) and binds; positional ops are served (reads byte-exact against the host's pattern, writes acked); the engagement law holds (served == ring ops; the lane gate's over-threshold op is a counted passthrough)" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const fd = try rig.openBound(sys.O.RDWR);
    defer sys.close(fd);
    try testing.expectEqual(@as(u32, 1), rig.st.readySessions());
    try testing.expectEqual(@as(u64, 1), rig.host.binds.load(.monotonic));
    const pin = rig.st.table.lookup(fd) orelse return error.NotBound;
    try testing.expect(pin.cell.authoritative(rig.st.table.currentEpoch()));
    const session = rig.st.sessionAt(pin.cell.binding.session);
    _ = pin.unpin();
    var payload: [8192]u8 = undefined;
    for (&payload, 0..) |*b, i| b.* = @truncate(i * 7 + 3);
    try expectDone(ops.positional(rig.st, .write, fd, &payload, payload.len, 4096), payload.len);
    var out: [8192]u8 = undefined;
    try expectDone(ops.positional(rig.st, .read, fd, &out, out.len, 4096), out.len);
    try expectPattern(&out, 4096);
    try testing.expectEqual(@as(u64, 2), rig.served());
    // The shape rows are the real call, uncounted on the ring.
    try testing.expect(ops.positional(rig.st, .read, fd, &out, 0, 0) == .real);
    try testing.expect(ops.positional(rig.st, .read, fd, null, 8, 0) == .real);
    try testing.expect(ops.positional(rig.st, .read, fd, &out, 8, -1) == .real);
    try testing.expectEqual(@as(u64, 2), rig.served());
    // The lane gate: an op strictly above the threshold routes kernel.
    session.lane_gate_min = 4096;
    const before = classify.passthroughCount(.lane_gate);
    try testing.expect(ops.positional(rig.st, .read, fd, &out, 4097, 0) == .real);
    try testing.expectEqual(before + 1, classify.passthroughCount(.lane_gate));
    try testing.expectEqual(@as(u64, 1), session.statsPage().lane_gate_kernel_routes.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), session.statsPage().preload_passthrough_ops[@backingInt(classify.Reason.lane_gate)].load(.monotonic));
    try expectDone(ops.positional(rig.st, .read, fd, &out, 4096, 0), 4096);
    session.lane_gate_min = 0;
    // A second open on the same shard reuses the session; another fd number
    // may land on another shard and establish a second session.
    const fd2 = try rig.openBound(sys.O.RDONLY);
    defer sys.close(fd2);
    try testing.expectEqual(@as(u32, 1), rig.st.readySessions());
    // Wrong direction: the real call (the kernel's EBADF).
    const wd = classify.passthroughCount(.wrong_direction);
    try testing.expect(ops.positional(rig.st, .write, fd2, &out, 16, 0) == .real);
    try testing.expectEqual(wd + 1, classify.passthroughCount(.wrong_direction));
}

test "e2e: offsetful read/write ride the mirror (zero lseek syscalls between ops), lseek SEEK_CUR/SEEK_SET are arithmetic, SEEK_END re-seeds and keeps the arm, a dup shares the mirror and the LAST close sends the one unbind" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const fd = try rig.openBound(sys.O.RDWR);
    var payload: [1000]u8 = undefined;
    for (&payload, 0..) |*b, i| b.* = @truncate(i);
    try expectDone(ops.offsetful(rig.st, .write, fd, &payload, payload.len), payload.len);
    try expectDone(ops.offsetful(rig.st, .write, fd, &payload, payload.len), payload.len);
    // The kernel f_pos is still 0: the mirror carried both writes.
    try testing.expectEqual(@as(?u64, 0), sys.tell(fd));
    switch (ops.lseek(rig.st, fd, 0, linux.SEEK.CUR)) {
        .done => |v| try testing.expectEqual(@as(i64, 2000), v),
        .real => return error.TestExpectedServed,
    }
    switch (ops.lseek(rig.st, fd, 500, linux.SEEK.SET)) {
        .done => |v| try testing.expectEqual(@as(i64, 500), v),
        .real => return error.TestExpectedServed,
    }
    var out: [1000]u8 = undefined;
    try expectDone(ops.offsetful(rig.st, .read, fd, &out, 500), 500);
    try expectPattern(out[0..500], 500);
    switch (ops.lseek(rig.st, fd, -1, linux.SEEK.SET)) {
        .done => |v| {
            try testing.expectEqual(@as(i64, -1), v);
            try testing.expectEqual(linux.E.INVAL, sys.getErrno());
        },
        .real => return error.TestExpectedServed,
    }
    // SEEK_END: the kernel's answer (the scratch file is 1 MiB), the arm survives.
    switch (ops.lseek(rig.st, fd, 0, linux.SEEK.END)) {
        .done => |v| try testing.expectEqual(@as(i64, 1 << 20), v),
        .real => return error.TestExpectedServed,
    }
    const pin = rig.st.table.lookup(fd) orelse return error.NotBound;
    try testing.expect(pin.cell.authoritative(rig.st.table.currentEpoch()));
    try testing.expectEqual(@as(u64, 1 << 20), pin.cell.offset.load(.monotonic));
    _ = pin.unpin();
    // The dup shares the description's mirror.
    const d: sys.fd_t = @intCast(linux.dup(fd));
    ops.dupPost(rig.st, fd, d);
    switch (ops.lseek(rig.st, d, 0, linux.SEEK.SET)) {
        .done => |v| try testing.expectEqual(@as(i64, 0), v),
        .real => return error.TestExpectedServed,
    }
    try expectDone(ops.offsetful(rig.st, .read, fd, &out, 10), 10);
    switch (ops.lseek(rig.st, d, 0, linux.SEEK.CUR)) {
        .done => |v| try testing.expectEqual(@as(i64, 10), v),
        .real => return error.TestExpectedServed,
    }
    // Close the original: the dup stays bound; the last close unbinds once.
    ops.closePre(rig.st, fd);
    sys.close(fd);
    const pd = rig.st.table.lookup(d) orelse return error.NotBound;
    try testing.expect(pd.unpin() == null);
    try expectDone(ops.offsetful(rig.st, .read, d, &out, 10), 10);
    ops.closePre(rig.st, d);
    sys.close(d);
    try testing.expectEqual(@as(u32, 0), rig.st.table.boundCount());
    // The UNBIND reaches the host.
    const deadline = clock.monotonicNs() + 2 * std.time.ns_per_s;
    while (rig.host.unbinds.load(.monotonic) < 1 and clock.monotonicNs() < deadline) sys.nanosleep(std.time.ns_per_ms);
    try testing.expectEqual(@as(u64, 1), rig.host.unbinds.load(.monotonic));
}

test "e2e: vector ops serve segment by segment with one f_pos advance; an RWF flag outside the allow-list demotes and takes the real call; the hygiene sweep releases a raw-closed number; close_range sweeps" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const fd = try rig.openBound(sys.O.RDWR);
    var a: [300]u8 = @splat(0xa1);
    var b: [700]u8 = @splat(0xb2);
    var iov = [_]ops.Iovec{ .{ .base = &a, .len = a.len }, .{ .base = null, .len = 0 }, .{ .base = &b, .len = b.len } };
    try expectDone(ops.vector(rig.st, .write, fd, &iov, iov.len, null, 0), 1000);
    switch (ops.lseek(rig.st, fd, 0, linux.SEEK.CUR)) {
        .done => |v| try testing.expectEqual(@as(i64, 1000), v),
        .real => return error.TestExpectedServed,
    }
    var oa: [300]u8 = undefined;
    var ob: [700]u8 = undefined;
    var oiov = [_]ops.Iovec{ .{ .base = &oa, .len = oa.len }, .{ .base = &ob, .len = ob.len } };
    try expectDone(ops.vector(rig.st, .read, fd, &oiov, oiov.len, 0, 0), 1000);
    try expectPattern(&oa, 0);
    try expectPattern(&ob, 300);
    // RWF_NOWAIT: the offsetful shape demotes (the kernel f_pos catches up) and the real call runs.
    const before = classify.passthroughCount(.rwf_flags);
    try testing.expect(ops.vector(rig.st, .write, fd, &iov, iov.len, null, linux.RWF.NOWAIT) == .real);
    try testing.expectEqual(before + 1, classify.passthroughCount(.rwf_flags));
    try testing.expectEqual(@as(?u64, 1000), sys.tell(fd));
    const pin = rig.st.table.lookup(fd) orelse return error.NotBound;
    try testing.expect(!pin.cell.authoritative(rig.st.table.currentEpoch()));
    _ = pin.unpin();
    // Demoted: an offsetful op still serves, now from the kernel f_pos.
    try expectDone(ops.offsetful(rig.st, .read, fd, &oa, 100), 100);
    try testing.expectEqual(@as(?u64, 1100), sys.tell(fd));
    // A raw close + the number coming back from socket(): the sweep releases it.
    sys.close(fd);
    const sock: sys.fd_t = @intCast(linux.socket(linux.AF.UNIX, linux.SOCK.DGRAM | linux.SOCK.CLOEXEC, 0));
    try testing.expectEqual(fd, sock);
    try testing.expect(rig.st.table.isBound(sock));
    ops.sweep(rig.st, sock);
    try testing.expect(!rig.st.table.isBound(sock));
    sys.close(sock);
    // close_range over a bound fd and an unallocated tail.
    const f2 = try rig.openBound(sys.O.RDWR);
    ops.closeRangePre(rig.st, @intCast(f2), std.math.maxInt(u32), 0);
    try testing.expect(!rig.st.table.isBound(f2));
    sys.close(f2);
    const f3 = try rig.openBound(sys.O.RDWR);
    const cloexec: u32 = @bitCast(linux.CLOSE_RANGE{ .UNSHARE = false, .CLOEXEC = true });
    ops.closeRangePre(rig.st, @intCast(f3), std.math.maxInt(u32), cloexec);
    try testing.expect(rig.st.table.isBound(f3));
    ops.closePre(rig.st, f3);
    sys.close(f3);
}

test "e2e: fcntl F_SETLK / F_SETFL+O_APPEND unbind after flushing the mirror; mmap MAP_SHARED unbinds every sibling of the inode and poisons it for the process; MAP_PRIVATE on a bound fd unbinds but does not poison; the fork flush demotes and the child drops everything" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const fd = try rig.openBound(sys.O.RDWR);
    var buf: [64]u8 = @splat(1);
    try expectDone(ops.offsetful(rig.st, .write, fd, &buf, buf.len), buf.len);
    const before = classify.passthroughCount(.fcntl_unbind);
    ops.fcntlPre(rig.st, fd, linux.F.SETLK, 0);
    try testing.expect(!rig.st.table.isBound(fd));
    try testing.expectEqual(before + 1, classify.passthroughCount(.fcntl_unbind));
    // The mirror was flushed: the kernel f_pos is past the write.
    try testing.expectEqual(@as(?u64, 64), sys.tell(fd));
    sys.close(fd);
    // F_SETFL adding O_APPEND.
    const f2 = try rig.openBound(sys.O.RDWR);
    ops.fcntlPre(rig.st, f2, linux.F.SETFL, sys.O.APPEND);
    try testing.expect(!rig.st.table.isBound(f2));
    // F_SETFL without a class change keeps the binding.
    const f2b = try rig.openBound(sys.O.RDWR);
    ops.fcntlPre(rig.st, f2b, linux.F.SETFL, 0);
    try testing.expect(rig.st.table.isBound(f2b));
    ops.closePre(rig.st, f2b);
    sys.close(f2b);
    sys.close(f2);
    // F_SETFL toggling O_NOATIME: the daemon captured the description's
    // atime exemption at bind — unbind, either way.
    const f2c = try rig.openBound(sys.O.RDWR);
    ops.fcntlPre(rig.st, f2c, linux.F.SETFL, sys.O.NOATIME);
    try testing.expect(!rig.st.table.isBound(f2c));
    sys.close(f2c);
    const f2d = try rig.openBound(sys.O.RDWR | sys.O.NOATIME);
    ops.fcntlPre(rig.st, f2d, linux.F.SETFL, sys.O.NOATIME);
    try testing.expect(rig.st.table.isBound(f2d));
    ops.fcntlPre(rig.st, f2d, linux.F.SETFL, 0);
    try testing.expect(!rig.st.table.isBound(f2d));
    sys.close(f2d);
    // mmap MAP_SHARED on one of two fds of the inode: both unbind, the
    // inode never binds again.
    const g1 = try rig.openBound(sys.O.RDWR);
    const g2 = try rig.openBound(sys.O.RDONLY);
    const mm_before = classify.passthroughCount(.mmap_unbind);
    const MAP_SHARED: c_int = 0x01;
    ops.mmapPre(rig.st, g1, MAP_SHARED);
    try testing.expect(!rig.st.table.isBound(g1));
    try testing.expect(!rig.st.table.isBound(g2));
    try testing.expectEqual(mm_before + 1, classify.passthroughCount(.mmap_unbind));
    const st = sys.fstat(g1).?;
    try testing.expect(rig.st.mapped.contains(st.dev, st.ino));
    const epoch = rig.st.table.currentEpoch();
    const g3 = try rig.open(sys.O.RDWR);
    const mi = classify.passthroughCount(.mapped_inode);
    try testing.expectEqual(classify.Verdict.passthrough, classify.classifyAndBind(rig.st, Mount.probes, g3, epoch));
    try testing.expectEqual(mi + 1, classify.passthroughCount(.mapped_inode));
    sys.close(g1);
    sys.close(g2);
    sys.close(g3);
    // A second scratch file (another inode): MAP_PRIVATE unbinds, no poison.
    rig.st.mapped = .{};
    const h = try rig.openBound(sys.O.RDWR);
    const MAP_PRIVATE: c_int = 0x02;
    ops.mmapPre(rig.st, h, MAP_PRIVATE);
    try testing.expect(!rig.st.table.isBound(h));
    try testing.expect(!rig.st.mapped.contains(st.dev, st.ino));
    sys.close(h);
    // The fork flush: an armed mirror's offset reaches the kernel; the
    // epoch moved; the child's bookkeeping drops every slot.
    const k = try rig.openBound(sys.O.RDWR);
    try expectDone(ops.offsetful(rig.st, .write, k, &buf, buf.len), buf.len);
    try testing.expectEqual(@as(?u64, 0), sys.tell(k));
    const e0 = rig.st.table.currentEpoch();
    ops.spawnPre(rig.st);
    try testing.expectEqual(e0 + 1, rig.st.table.currentEpoch());
    try testing.expectEqual(@as(?u64, 64), sys.tell(k));
    const pk = rig.st.table.lookup(k) orelse return error.NotBound;
    try testing.expect(!pk.cell.authoritative(rig.st.table.currentEpoch()));
    _ = pk.unpin();
    // Still served, from the kernel f_pos now.
    try expectDone(ops.offsetful(rig.st, .write, k, &buf, buf.len), buf.len);
    try testing.expectEqual(@as(?u64, 128), sys.tell(k));
    rig.st.table.clearAllInChild();
    try testing.expect(!rig.st.table.isBound(k));
    sys.close(k);
}

test "e2e: the daemon dying mid-op — a frozen, severed host ⇒ the op falls through to the real call inside the heartbeat ladder, the session is poisoned, the fd is released without ctl traffic, and every later op on the mount is passthrough" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    try rig.st.env.put("SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS", "20000");
    const fd = try rig.openBound(sys.O.RDWR);
    defer sys.close(fd);
    var buf: [64]u8 = @splat(5);
    try expectDone(ops.positional(rig.st, .write, fd, &buf, buf.len, 0), buf.len);
    rig.host.sever.store(true, .release);
    rig.host.frozen.store(true, .release);
    sys.nanosleep(20 * std.time.ns_per_ms);
    const before = classify.passthroughCount(.poisoned);
    const t0 = clock.monotonicNs();
    try testing.expect(ops.positional(rig.st, .read, fd, &buf, buf.len, 0) == .real);
    const elapsed = clock.monotonicNs() - t0;
    // The heartbeat poisoned it, never the 20 s op deadline set above (the
    // only other way this op ends).
    try testing.expect(elapsed < 20_000 * std.time.ns_per_ms);
    try testing.expectEqual(before + 1, classify.passthroughCount(.poisoned));
    try testing.expect(!rig.st.table.isBound(fd));
    try testing.expectEqual(@as(u64, 0), rig.host.unbinds.load(.monotonic));
    // The real call on the fd: the kernel's answer (the scratch file).
    try testing.expectEqual(@as(usize, buf.len), linux.pread(fd, &buf, buf.len, 0));
    // A new open on the mount: the poisoned session refuses the bind
    // silently; the open stays kernel-served.
    const epoch = rig.st.table.currentEpoch();
    const f2 = try rig.open(sys.O.RDWR);
    defer sys.close(f2);
    const p2 = classify.passthroughCount(.poisoned);
    try testing.expectEqual(classify.Verdict.passthrough, classify.classifyAndBind(rig.st, Mount.probes, f2, epoch));
    try testing.expectEqual(p2 + 1, classify.passthroughCount(.poisoned));
}

test "e2e: libaio over the ring — 8 iocbs in one io_submit (6 ring, 2 kernel-class) are accepted in order, io_getevents merges them with exact cookies and the host's pattern byte-exact, io_cancel of a ring op answers −EINPROGRESS, io_destroy abandons" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const fd = try rig.openBound(sys.O.RDWR);
    defer sys.close(fd);
    const plain: sys.fd_t = @intCast(linux.openat(linux.AT.FDCWD, "/dev/null", .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0));
    defer sys.close(plain);
    // The real libaio lane: the raw syscalls (libaio's functions ARE the
    // kernel's with the −errno convention).
    const Raw = struct {
        fn submit(ctx: aio.io_context_t, nr: c_long, ios: [*]const ?*aio.Iocb) callconv(.c) c_int {
            return sys.aioResult(linux.syscall3(.io_submit, ctx, @bitCast(nr), @intFromPtr(ios)));
        }
        fn getevents(ctx: aio.io_context_t, min: c_long, nr: c_long, ev: [*]aio.IoEvent, ts: ?*linux.timespec) callconv(.c) c_int {
            return sys.aioResult(linux.syscall5(.io_getevents, ctx, @bitCast(min), @bitCast(nr), @intFromPtr(ev), @intFromPtr(ts)));
        }
    };
    const fns: aio.RealFns = .{ .io_submit = Raw.submit, .io_getevents = Raw.getevents };
    var ctx: aio.io_context_t = 0;
    try testing.expectEqual(@as(c_int, 0), sys.aioResult(linux.syscall2(.io_setup, 16, @intFromPtr(&ctx))));
    aio.setupPost(rig.st, 16, &ctx, fns);
    defer {
        aio.destroyPre(rig.st, ctx, fns);
        _ = linux.syscall1(.io_destroy, ctx);
    }
    var bufs: [8][512]u8 = undefined;
    var iocbs: [8]aio.Iocb = undefined;
    var ptrs: [8]?*aio.Iocb = undefined;
    for (&iocbs, 0..) |*io, i| {
        for (&bufs[i]) |*b| b.* = @truncate(i + 11);
        const kernel_class = (i == 3 or i == 6);
        io.* = .{ .data = 1000 + i, .key = 0, .aio_rw_flags = 0, .aio_lio_opcode = aio.IOCB_CMD_PWRITE, .aio_reqprio = 0, .aio_fildes = if (kernel_class) plain else fd, .buf = @intFromPtr(&bufs[i]), .nbytes = 512, .offset = @intCast(i * 512), .__pad3 = 0, .flags = 0, .resfd = 0 };
        ptrs[i] = io;
    }
    switch (aio.submit(rig.st, ctx, 8, &ptrs, fns)) {
        .done => |n| try testing.expectEqual(@as(c_int, 8), n),
        .real => return error.TestExpectedServed,
    }
    var events: [8]aio.IoEvent = undefined;
    var got: usize = 0;
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (got < 8 and clock.monotonicNs() < deadline) {
        var ts: linux.timespec = .{ .sec = 1, .nsec = 0 };
        switch (aio.getevents(rig.st, ctx, 1, @intCast(8 - got), events[got..].ptr, &ts, fns)) {
            .done => |n| {
                try testing.expect(n >= 0);
                got += @intCast(n);
            },
            .real => return error.TestExpectedServed,
        }
    }
    try testing.expectEqual(@as(usize, 8), got);
    var seen: [8]bool = @splat(false);
    for (events) |e| {
        const i: usize = @intCast(e.data - 1000);
        try testing.expect(!seen[i]);
        seen[i] = true;
        try testing.expectEqual(@intFromPtr(&iocbs[i]), e.obj);
        try testing.expectEqual(@as(i64, 512), e.res);
    }
    // Reads through the ring with cookies; cancel answers −EINPROGRESS.
    var rbuf: [2][512]u8 = undefined;
    var rio: [2]aio.Iocb = undefined;
    var rptrs: [2]?*aio.Iocb = undefined;
    for (&rio, 0..) |*io, i| {
        io.* = .{ .data = 7 + i, .key = 0, .aio_rw_flags = 0, .aio_lio_opcode = aio.IOCB_CMD_PREAD, .aio_reqprio = 0, .aio_fildes = fd, .buf = @intFromPtr(&rbuf[i]), .nbytes = 512, .offset = @intCast(i * 512), .__pad3 = 0, .flags = 0, .resfd = 0 };
        rptrs[i] = io;
    }
    switch (aio.submit(rig.st, ctx, 2, &rptrs, fns)) {
        .done => |n| try testing.expectEqual(@as(c_int, 2), n),
        .real => return error.TestExpectedServed,
    }
    // Still pending ⇒ −EINPROGRESS; already reaped by the host ⇒ the real call.
    switch (aio.cancel(ctx, &rio[0])) {
        .done => |v| try testing.expectEqual(-@as(c_int, @backingInt(linux.E.INPROGRESS)), v),
        .real => {},
    }
    got = 0;
    while (got < 2 and clock.monotonicNs() < deadline) {
        var ts: linux.timespec = .{ .sec = 1, .nsec = 0 };
        switch (aio.getevents(rig.st, ctx, 2, 2, events[got..].ptr, &ts, fns)) {
            .done => |n| got += @intCast(@max(n, 0)),
            .real => return error.TestExpectedServed,
        }
    }
    try testing.expectEqual(@as(usize, 2), got);
    try expectPattern(&rbuf[0], 0);
    try expectPattern(&rbuf[1], 512);
    // Every ring op accounted on the host.
    try testing.expectEqual(@as(u64, 8), rig.served());
}

/// The raw libaio lane the e2e tests stand in for the real symbols with.
const RawAio = struct {
    fn submit(ctx: aio.io_context_t, nr: c_long, ios: [*]const ?*aio.Iocb) callconv(.c) c_int {
        return sys.aioResult(linux.syscall3(.io_submit, ctx, @bitCast(nr), @intFromPtr(ios)));
    }
    fn getevents(ctx: aio.io_context_t, min: c_long, nr: c_long, ev: [*]aio.IoEvent, ts: ?*linux.timespec) callconv(.c) c_int {
        return sys.aioResult(linux.syscall5(.io_getevents, ctx, @bitCast(min), @bitCast(nr), @intFromPtr(ev), @intFromPtr(ts)));
    }
    const fns: aio.RealFns = .{ .io_submit = submit, .io_getevents = getevents };
};

fn mkWrite(fd: sys.fd_t, buf: []u8, offset: i64, cookie: u64) aio.Iocb {
    return .{ .data = cookie, .key = 0, .aio_rw_flags = 0, .aio_lio_opcode = aio.IOCB_CMD_PWRITE, .aio_reqprio = 0, .aio_fildes = fd, .buf = @intFromPtr(buf.ptr), .nbytes = buf.len, .offset = offset, .__pad3 = 0, .flags = 0, .resfd = 0 };
}

test "e2e: fio's shape — a context from libaio's io_queue_init rides the ring (its iocbs on a bound fd are served, served == ops); a context the shim never registered is a COUNTED passthrough: per bound iocb at io_submit on the binding's stats page, per call at io_getevents/io_cancel" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const fd = try rig.openBound(sys.O.RDWR);
    defer sys.close(fd);
    const pin = rig.st.table.lookup(fd) orelse return error.NotBound;
    const session = rig.st.sessionAt(pin.cell.binding.session);
    _ = pin.unpin();
    // The queue face: the context is registered by the shim's own io_setup.
    var ctx: aio.io_context_t = 0;
    try testing.expectEqual(@as(c_int, 0), interpose.wrappers.io_queue_init(16, &ctx));
    try testing.expect(aio.registry.lookup(ctx) != null);
    var bufs: [4][512]u8 = undefined;
    var iocbs: [4]aio.Iocb = undefined;
    var ptrs: [4]?*aio.Iocb = undefined;
    for (&iocbs, 0..) |*io, i| {
        @memset(&bufs[i], @truncate(i + 1));
        io.* = mkWrite(fd, &bufs[i], @intCast(i * 512), 500 + i);
        ptrs[i] = io;
    }
    switch (aio.submit(rig.st, ctx, 4, &ptrs, RawAio.fns)) {
        .done => |n| try testing.expectEqual(@as(c_int, 4), n),
        .real => return error.TestExpectedServed,
    }
    // The reap IS the rendezvous on the completions: each call parks on the
    // session's completion doorbell (register → snapshot → re-scan, 5 ms
    // re-check) and returns as soon as one lands; the outer bound only
    // keeps a lost completion from wedging the row — on expiry the host's
    // own account says whether it was lost before or after the ring.
    var events: [4]aio.IoEvent = undefined;
    var got: usize = 0;
    const deadline = clock.monotonicNs() + REAP_BOUND_NS;
    while (got < 4 and clock.monotonicNs() < deadline) {
        var ts: linux.timespec = .{ .sec = 1, .nsec = 0 };
        switch (aio.getevents(rig.st, ctx, 1, @intCast(4 - got), events[got..].ptr, &ts, RawAio.fns)) {
            .done => |n| got += @intCast(@max(n, 0)),
            .real => return error.TestExpectedServed,
        }
    }
    if (got != 4) std.debug.print("\nfio's shape: {d} of 4 ring completions reaped within {d} s — host served {d}, park eras {d}, abandoned slots {d}\n", .{ got, REAP_BOUND_NS / std.time.ns_per_s, rig.served(), session.statsPage().il_park_eras.load(.monotonic), session.il_abandoned_slots.load(.monotonic) });
    try testing.expectEqual(@as(usize, 4), got);
    // The host's account is published WITH each completion (its `served`
    // bump precedes the DONE release): exact on the last reap's heels.
    try testing.expectEqual(@as(u64, 4), rig.served());
    try testing.expectEqual(@as(c_int, 0), interpose.wrappers.io_queue_release(ctx));
    try testing.expect(aio.registry.lookup(ctx) == null);
    // An unregistered context (the raw syscall, as a static libaio or a
    // registry overflow would): the real call, counted.
    var raw_ctx: aio.io_context_t = 0;
    try testing.expectEqual(@as(c_int, 0), sys.aioResult(linux.syscall2(.io_setup, 16, @intFromPtr(&raw_ctx))));
    defer _ = linux.syscall1(.io_destroy, raw_ctx);
    try testing.expect(aio.registry.lookup(raw_ctx) == null);
    const plain: sys.fd_t = @intCast(linux.openat(linux.AT.FDCWD, "/dev/null", .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0));
    defer sys.close(plain);
    var unbound = mkWrite(plain, &bufs[3], 0, 900);
    ptrs[3] = &unbound;
    const op_before = classify.passthroughCount(.aio_unknown_ctx_op);
    const call_before = classify.passthroughCount(.aio_unknown_ctx_call);
    const page_before = session.statsPage().preload_passthrough_ops[@backingInt(classify.Reason.aio_unknown_ctx_op)].load(.monotonic);
    try testing.expect(aio.submit(rig.st, raw_ctx, 4, &ptrs, RawAio.fns) == .real);
    // Three iocbs on the bound fd count (on its session's page); the
    // unbound one is not ours.
    try testing.expectEqual(op_before + 3, classify.passthroughCount(.aio_unknown_ctx_op));
    try testing.expectEqual(page_before + 3, session.statsPage().preload_passthrough_ops[@backingInt(classify.Reason.aio_unknown_ctx_op)].load(.monotonic));
    try testing.expectEqual(call_before, classify.passthroughCount(.aio_unknown_ctx_call));
    var ts0: linux.timespec = .{ .sec = 0, .nsec = 0 };
    try testing.expect(aio.getevents(rig.st, raw_ctx, 0, 4, &events, &ts0, RawAio.fns) == .real);
    try testing.expect(aio.cancel(raw_ctx, &iocbs[0]) == .real);
    try testing.expectEqual(call_before + 2, classify.passthroughCount(.aio_unknown_ctx_call));
    // Nothing of the passthrough reached the ring.
    try testing.expectEqual(@as(u64, 4), rig.served());
}

/// A `struct fiemap` followed by room for `cap` extents, as an app lays
/// out the FS_IOC_FIEMAP argument.
fn FiemapArg(comptime cap: usize) type {
    return extern struct {
        h: ops_fiemap.Fiemap,
        e: [cap]ops_fiemap.Extent,
    };
}

const ops_fiemap = @import("../fuse/handler/fiemap.zig");

fn expectFiemapDone(v: ops.Verdict(c_int), want: c_int) !void {
    switch (v) {
        .done => |n| try testing.expectEqual(want, n),
        .real => return error.TestExpectedServed,
    }
}

test "e2e: FS_IOC_FIEMAP through the verb follows ioctl_fiemap's copy law — 3,000 extents arrive in three batches (1,169 + 1,169 + 662; SYNC asked once) with LAST on the last; fm_extent_count 0 counts them and writes no extent; a short array stops without LAST; XATTR is EBADR with fm_flags = XATTR read back; a NULL or unmapped argument is EFAULT and an array running into an unmapped page keeps the extents written before it; O_PATH and a directory are the real call" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const fd = try rig.openBound(sys.O.RDWR);
    defer sys.close(fd);
    rig.host.fiemap_extents = 3000;
    const fm = ops_fiemap;
    const Big = FiemapArg(3000);
    const big = try testing.allocator.create(Big);
    defer testing.allocator.destroy(big);
    big.h = .{ .fm_start = 0, .fm_length = std.math.maxInt(u64), .fm_flags = fm.FIEMAP_FLAG_SYNC, .fm_mapped_extents = 0, .fm_extent_count = 3000, .fm_reserved = 0 };
    try expectFiemapDone(ops.fiemapIoctl(rig.st, Mount.probes, fd, @intFromPtr(big)), 0);
    try testing.expectEqual(@as(u32, 3000), big.h.fm_mapped_extents);
    try testing.expectEqual(fm.FIEMAP_FLAG_SYNC, big.h.fm_flags);
    try testing.expectEqual(@as(u64, std.math.maxInt(u64)), big.h.fm_length);
    for (big.e, 0..) |e, i| {
        try testing.expectEqual(@as(u64, i * 8192), e.fe_logical);
        try testing.expectEqual(@as(u64, 4096), e.fe_length);
        try testing.expectEqual(if (i == 2999) fm.FIEMAP_EXTENT_LAST else 0, e.fe_flags);
    }
    try testing.expectEqual(@as(u32, 3), rig.host.fiemap_asks.load(.monotonic));
    try testing.expectEqual(@as(u32, 1), rig.host.fiemap_sync_asks.load(.monotonic));
    // The count alone: no extent written.
    var c: FiemapArg(1) = undefined;
    c.h = .{ .fm_start = 0, .fm_length = std.math.maxInt(u64), .fm_flags = 0, .fm_mapped_extents = 99, .fm_extent_count = 0, .fm_reserved = 0 };
    c.e[0].fe_length = 0xDEAD;
    try expectFiemapDone(ops.fiemapIoctl(rig.st, Mount.probes, fd, @intFromPtr(&c)), 0);
    try testing.expectEqual(@as(u32, 3000), c.h.fm_mapped_extents);
    try testing.expectEqual(@as(u64, 0xDEAD), c.e[0].fe_length);
    // A short array: ten extents, the tenth not LAST.
    var ten: FiemapArg(10) = undefined;
    ten.h = .{ .fm_start = 8192 * 5, .fm_length = std.math.maxInt(u64), .fm_flags = 0, .fm_mapped_extents = 0, .fm_extent_count = 10, .fm_reserved = 0 };
    try expectFiemapDone(ops.fiemapIoctl(rig.st, Mount.probes, fd, @intFromPtr(&ten)), 0);
    try testing.expectEqual(@as(u32, 10), ten.h.fm_mapped_extents);
    try testing.expectEqual(@as(u64, 8192 * 5), ten.e[0].fe_logical);
    try testing.expectEqual(@as(u32, 0), ten.e[9].fe_flags & fm.FIEMAP_EXTENT_LAST);
    // XATTR: EBADR, fm_flags = the refused bit, nothing mapped.
    ten.h = .{ .fm_start = 0, .fm_length = 1 << 20, .fm_flags = fm.FIEMAP_FLAG_XATTR | fm.FIEMAP_FLAG_SYNC, .fm_mapped_extents = 7, .fm_extent_count = 10, .fm_reserved = 0 };
    try expectFiemapDone(ops.fiemapIoctl(rig.st, Mount.probes, fd, @intFromPtr(&ten)), -1);
    try testing.expectEqual(linux.E.BADR, sys.getErrno());
    try testing.expectEqual(fm.FIEMAP_FLAG_XATTR, ten.h.fm_flags);
    try testing.expectEqual(@as(u32, 0), ten.h.fm_mapped_extents);
    // A zero length: EINVAL, the header read back unchanged but the count.
    ten.h = .{ .fm_start = 0, .fm_length = 0, .fm_flags = 0, .fm_mapped_extents = 7, .fm_extent_count = 10, .fm_reserved = 0 };
    try expectFiemapDone(ops.fiemapIoctl(rig.st, Mount.probes, fd, @intFromPtr(&ten)), -1);
    try testing.expectEqual(linux.E.INVAL, sys.getErrno());
    try testing.expectEqual(@as(u32, 0), ten.h.fm_mapped_extents);
    // EFAULT: NULL, an unmapped page, an array running into one.
    try expectFiemapDone(ops.fiemapIoctl(rig.st, Mount.probes, fd, 0), -1);
    try testing.expectEqual(linux.E.FAULT, sys.getErrno());
    const pages = sys.mapAnon(8192).?;
    sys.unmapAnon(@alignCast(pages + 4096), 4096);
    defer sys.unmapAnon(pages, 4096);
    try expectFiemapDone(ops.fiemapIoctl(rig.st, Mount.probes, fd, @intFromPtr(pages) + 4096), -1);
    try testing.expectEqual(linux.E.FAULT, sys.getErrno());
    // Room for (4096 − 32 − 64) / 56 = 71 whole extents before the hole.
    const at = @intFromPtr(pages) + 64;
    const h: fm.Fiemap = .{ .fm_start = 0, .fm_length = std.math.maxInt(u64), .fm_flags = 0, .fm_mapped_extents = 0, .fm_extent_count = 100, .fm_reserved = 0 };
    @memcpy(pages[64..][0..@sizeOf(fm.Fiemap)], std.mem.asBytes(&h));
    try expectFiemapDone(ops.fiemapIoctl(rig.st, Mount.probes, fd, at), -1);
    try testing.expectEqual(linux.E.FAULT, sys.getErrno());
    const back = std.mem.bytesToValue(fm.Fiemap, pages[64..][0..@sizeOf(fm.Fiemap)]);
    try testing.expectEqual(@as(u32, (4096 - 64 - 32) / 56), back.fm_mapped_extents);
    // Not ours to serve: O_PATH, a directory.
    const opath = try rig.open(sys.O.PATH);
    defer sys.close(opath);
    try testing.expect(ops.fiemapIoctl(rig.st, Mount.probes, opath, @intFromPtr(&ten)) == .real);
    const dir: sys.fd_t = @intCast(linux.openat(linux.AT.FDCWD, "/tmp", .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0));
    defer sys.close(dir);
    try testing.expect(ops.fiemapIoctl(rig.st, Mount.probes, dir, @intFromPtr(&ten)) == .real);
}
