//! THE FIDELITY TIER (charter §8 "a fidelity tier stands the fabric up
//! through the product's own verbs with zero mocks", §11 release gate;
//! `docs/porting/ops_nvmet_keys.md` §5): `zig build nvmet-smoke` runs the
//! library's `nvmet fidelity:` tests with the built binary as
//! `./squeezefs` in the cwd (the `smoke` pattern) against the venue's
//! REAL kernel nvmet target — nvmet-loop for the latency-shaped rows,
//! nvmet-tcp on 127.0.0.1 for anything fabric-sensitive — with NO nvme-cli,
//! no modprobe, no losetup: every assertion the reference made with
//! `nvme`/`jq` is made through `nvmeof list --json`, `nvmeof resv-report
//! --json`, `.stats` and the ledger (quirk Q14). Every object carries the
//! harness marker `:fideli-` (refused by `adopt`) and rides the port-id
//! slice 54000–54999 (the allocator probes past another run's ports); every
//! tcp listener takes a TCP port of the run's own (`Rig.tcpPort`, the
//! kernel's ephemeral choice held by a probe socket), so concurrent gate
//! runs on one venue never meet; teardown removes ONLY what the leg made; a leg ends
//! with a zero-residue census (none of its subsystems, ports, controllers
//! or records survive). Root, `/dev/nvme-fabrics`, `/dev/fuse` and the
//! 7.2.6-sqz venue are required; anything less skips LOUDLY.
//!
//! Legs: `roundtrip-{loop,tcp}` (G1), `pr-matrix-{loop,tcp}` + the
//! registration-survives-disconnect row (tcp), `fidelity-mount` (THE row
//! charter §8 names: the metadata and data volumes ARE connected
//! namespaces; PR armed for real; a second identity refused; `kill -9` →
//! remount recovers through the own-stale ladder; `umount` → zero
//! registrants), `sigstop-preempt` (a stopped holder is preempted past
//! `T_owner`; resumed, its next beat reads the usurpation and its barrier
//! answers the device's `-EBADE` — FAILED), `g2-restore-tcp` (the PTPL
//! lapse: the subsystem wiped by hand under a live mount, `restore`
//! rebuilds it with the recorded uuid, the heartbeat re-acquires:
//! `writer_guard_pr_reacquires` grows, `writer_guard_fenced` stays 0),
//! `crash-window` (law 6 on the real tree), `adopt`, `pr-registrants` (64
//! real registrants: the two-step report reads 64 + 64 × 64 B), `loudfail`
//! and `storage-list`.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const Allocator = std.mem.Allocator;

const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const json_scan = @import("../../core/json_scan.zig");
const checksum = @import("../../core/checksum.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const nvme = @import("../../meta/guard/nvme_cmd.zig");
const pr = @import("../../meta/guard/pr.zig");
const pr_linux = @import("../../meta/guard/pr_linux.zig");
const lease_clock = @import("../../meta/guard/lease_clock.zig");
const configfs = @import("configfs.zig");
const ledger = @import("ledger.zig");
const initiator = @import("initiator.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/fidelity is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const testing = std.testing;

const root_knob = "SQUEEZEFS_TEST_FIDELITY_ROOT";
const state_dir_knob = "SQUEEZEFS_NVMEOF_STATE_DIR";
const port_base_knob = "SQUEEZEFS_NVMET_PORT_ID_BASE";
const hostnqn_knob = "SQUEEZEFS_HOSTNQN";
const hostid_knob = "SQUEEZEFS_HOSTID";

/// The harness's own identities (RFC 4122-shaped, never a host's real
/// one): A rides the rig's base env (the venue container has an EMPTY
/// `/etc/machine-id`, so the derived default refuses there by design —
/// the first mount's identity is explicit here), B is the second
/// association's.
const ident_a: initiator.HostPair = .{ .hostnqn = "nqn.2014-08.org.nvmexpress:uuid:a0a0a0a0-a0a0-4a0a-8a0a-a0a0a0a0a0a0", .hostid = "a0a0a0a0-a0a0-4a0a-8a0a-a0a0a0a0a0a0" };
const ident_b: initiator.HostPair = .{ .hostnqn = "nqn.2014-08.org.nvmexpress:uuid:b0b0b0b0-b0b0-4b0b-8b0b-b0b0b0b0b0b0", .hostid = "b0b0b0b0-b0b0-4b0b-8b0b-b0b0b0b0b0b0" };

/// 127.0.0.1, the address every tcp leg listens and connects on.
const loopback_v4: u32 = 0x7f00_0001;
/// A meta volume of 1 GiB and a data volume of 2 GiB (sparse on ext4).
const meta_len: u64 = 1 << 30;
const data_len: u64 = 2 << 30;
/// 64 MiB of O_DIRECT round trip per G1 leg, 4 MiB per op.
const roundtrip_len: usize = 64 << 20;
const roundtrip_chunk: usize = 4 << 20;
const ready_wait_ns: u64 = 60 * std.time.ns_per_s;
const exit_wait_ns: u64 = 60 * std.time.ns_per_s;
/// The G2 bound: nvme-tcp's `reconnect_delay` (10 s) + one heartbeat
/// (10 s) + the admin scan, with margin.
const g2_wait_ns: u64 = 120 * std.time.ns_per_s;

const Connected = struct {
    subnqn: [256]u8 = undefined,
    subnqn_len: usize = 0,
    device: [40]u8 = undefined,
    device_len: usize = 0,
    char: [40]u8 = undefined,
    char_len: usize = 0,
    instance: u32 = 0,
    /// The explicit identity the connect rode (null = the rig's base pair).
    hostnqn_buf: [256]u8 = undefined,
    hostnqn_len: usize = 0,
    hostid_buf: [64]u8 = undefined,
    hostid_len: usize = 0,

    fn ident(self: *const Connected) ?initiator.HostPair {
        if (self.hostnqn_len == 0) return null;
        return .{ .hostnqn = self.hostnqn_buf[0..self.hostnqn_len], .hostid = self.hostid_buf[0..self.hostid_len] };
    }

    fn nqn(self: *const Connected) []const u8 {
        return self.subnqn[0..self.subnqn_len];
    }
    fn dev(self: *const Connected) []const u8 {
        return self.device[0..self.device_len];
    }
    fn chr(self: *const Connected) []const u8 {
        return self.char[0..self.char_len];
    }
};

const Rig = struct {
    gpa: Allocator,
    io: Io,
    bin: [:0]u8,
    root: [:0]u8,
    state: [:0]u8,
    env: std.process.Environ.Map,
    pool: *uring_fs.Pool,
    pid: linux.pid_t,
    /// The manifest: what this leg made and must remove.
    shares: std.ArrayList([]u8) = .empty,
    connects: std.ArrayList(Connected) = .empty,
    /// Live daemons (foreground mounts) with their log files.
    daemons: std.ArrayList(Daemon) = .empty,
    /// The probe sockets holding this run's TCP service ports (`tcpPort`).
    port_probes: std.ArrayList(linux.fd_t) = .empty,

    const Daemon = struct {
        child: std.process.Child,
        pid: linux.pid_t,
        log_path: [:0]u8,
        mp: [:0]u8,
        log_file: Io.File,
        /// Already waited (killed, umounted or died on its own).
        reaped: bool = false,
    };

    fn init(gpa: Allocator, io: Io) !*Rig {
        const self = try gpa.create(Rig);
        errdefer gpa.destroy(self);
        var proc_env = try std.testing.environ.createMap(gpa);
        defer proc_env.deinit();
        const base = knobs.present(proc_env.get(root_knob)) orelse "/build/fideli";
        const pid = linux.getpid();
        var cwd_buf: [linux.PATH_MAX]u8 = undefined;
        const n = Io.Dir.cwd().realPath(io, &cwd_buf) catch return error.NoCwd;
        self.* = .{
            .gpa = gpa,
            .io = io,
            .bin = try std.fmt.allocPrintSentinel(gpa, "{s}/squeezefs", .{cwd_buf[0..n]}, 0),
            .root = try std.fmt.allocPrintSentinel(gpa, "{s}-{d}", .{ base, pid }, 0),
            .state = try std.fmt.allocPrintSentinel(gpa, "{s}-{d}/state", .{ base, pid }, 0),
            .env = std.process.Environ.Map.init(gpa),
            .pool = undefined,
            .pid = pid,
        };
        try Io.Dir.cwd().createDirPath(io, self.root);
        try self.env.put("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        try self.env.put(state_dir_knob, self.state);
        try self.env.put(port_base_knob, "54000");
        try self.env.put(hostnqn_knob, ident_a.hostnqn);
        try self.env.put(hostid_knob, ident_a.hostid);
        self.pool = try uring_fs.Pool.init(gpa, uring_fs.Config.fixed(1, uring_fs.nofileSoftAsFound()));
        _ = linux.prctl(@backingInt(linux.PR.SET_CHILD_SUBREAPER), 1, 0, 0, 0);
        return self;
    }

    /// Teardown by manifest: daemons killed, controllers disconnected,
    /// shares unshared; then the residue census.
    fn deinit(self: *Rig) void {
        for (self.daemons.items) |*d| {
            if (!d.reaped) {
                _ = linux.kill(d.pid, .KILL);
                _ = d.child.wait(self.io) catch {};
                d.reaped = true;
            }
            _ = linux.umount2(d.mp.ptr, linux.MNT.DETACH);
            d.log_file.close(self.io);
            self.gpa.free(d.log_path);
            self.gpa.free(d.mp);
        }
        self.daemons.deinit(self.gpa);
        for (self.connects.items) |*c| disconnectQuiet(c);
        self.connects.deinit(self.gpa);
        for (self.shares.items) |s| {
            // The verb first; a share whose record a leg deleted (the
            // ledger-loss rows) is torn down by the walk — the harness's
            // own objects are its own to remove.
            var removed = false;
            if (self.sqz(&.{ "nvmeof", "unshare", s })) |r| {
                removed = r.term.success();
                self.freeRun(r);
            } else |_| {}
            if (!removed) self.teardownByWalk(s);
            self.gpa.free(s);
        }
        self.shares.deinit(self.gpa);
        for (self.port_probes.items) |fd| _ = linux.close(fd);
        self.port_probes.deinit(self.gpa);
        self.pool.deinit();
        Io.Dir.cwd().deleteTree(self.io, self.root) catch {};
        self.env.deinit();
        self.gpa.free(self.bin);
        self.gpa.free(self.root);
        self.gpa.free(self.state);
        self.gpa.destroy(self);
    }

    /// Teardown of a leg's subsystem straight through configfs (its live
    /// listeners' port ids; ports go last-out), for objects whose record
    /// a leg removed on purpose.
    fn teardownByWalk(self: *Rig, nqn: []const u8) void {
        const fs = configfs.RealFs.face();
        var walk = configfs.walkLive(fs, self.gpa) catch return;
        defer walk.deinit(self.gpa);
        const live = walk.find(nqn) orelse return;
        var ids: [16]u16 = undefined;
        var n: usize = 0;
        for (live.listeners) |l| if (n < ids.len) {
            ids[n] = l.port_id;
            n += 1;
        };
        _ = configfs.teardown(fs, self.gpa, .{ .subnqn = nqn, .port_ids = ids[0..n], .allow_hosts = &.{}, .host_named_elsewhere = struct {
            fn f(_: *const anyopaque, _: []const u8) bool {
                return true;
            }
        }.f, .ctx = @ptrCast(self) }) catch {};
    }

    fn sqz(self: *Rig, argv_tail: []const []const u8) !std.process.RunResult {
        return self.sqzEnv(argv_tail, &self.env);
    }

    fn sqzEnv(self: *Rig, argv_tail: []const []const u8, env: *const std.process.Environ.Map) !std.process.RunResult {
        var argv: [24][]const u8 = undefined;
        argv[0] = self.bin;
        for (argv_tail, 0..) |a, i| argv[i + 1] = a;
        return std.process.run(self.gpa, self.io, .{ .argv = argv[0 .. argv_tail.len + 1], .environ_map = env });
    }

    fn freeRun(self: *Rig, r: std.process.RunResult) void {
        self.gpa.free(r.stdout);
        self.gpa.free(r.stderr);
    }

    /// Run and REQUIRE success; both streams printed on failure.
    fn sqzOk(self: *Rig, argv_tail: []const []const u8) ![]u8 {
        const r = try self.sqz(argv_tail);
        defer self.gpa.free(r.stderr);
        if (!r.term.success()) {
            std.debug.print("\n--- squeezefs {s} failed ({f}) ---\n--- stdout ---\n{s}\n--- stderr ---\n{s}\n", .{ argv_tail[0], r.term, r.stdout, r.stderr });
            self.gpa.free(r.stdout);
            return error.VerbFailed;
        }
        return r.stdout;
    }

    /// An env with the second identity pair.
    fn envWith(self: *Rig, ident: initiator.HostPair) !std.process.Environ.Map {
        var e = std.process.Environ.Map.init(self.gpa);
        errdefer e.deinit();
        var it = self.env.iterator();
        while (it.next()) |kv| try e.put(kv.key_ptr.*, kv.value_ptr.*);
        try e.put(hostnqn_knob, ident.hostnqn);
        try e.put(hostid_knob, ident.hostid);
        return e;
    }

    fn nqnFor(self: *Rig, buf: *[256]u8, name: []const u8) []const u8 {
        return std.fmt.bufPrint(buf, "nqn.2026-10.io.squeezefs:fideli-{d}-{s}", .{ self.pid, name }) catch unreachable;
    }

    /// A TCP service port of this run's own: the kernel's ephemeral choice
    /// (`bind(127.0.0.1:0)` + `getsockname`), its probe socket held bound —
    /// SO_REUSEADDR, never listening — until the rig ends, so no concurrent
    /// run's `bind(0)` is handed it; nvmet-tcp's listener binds it beside
    /// the probe (`nvmet_tcp_add_port` sets SO_REUSEADDR, and only a
    /// LISTENING holder conflicts). Two gate runs on one venue (several
    /// worktrees) never meet on a port — the fixed ports they shared did.
    fn tcpPort(self: *Rig) !u16 {
        const rc = linux.socket(linux.AF.INET, linux.SOCK.STREAM | linux.SOCK.CLOEXEC, 0);
        if (linux.errno(rc) != .SUCCESS) return error.PortProbeFailed;
        const fd: linux.fd_t = @intCast(rc);
        errdefer _ = linux.close(fd);
        const on: c_int = 1;
        if (linux.errno(linux.setsockopt(fd, linux.SOL.SOCKET, linux.SO.REUSEADDR, std.mem.asBytes(&on), @sizeOf(c_int))) != .SUCCESS) return error.PortProbeFailed;
        var addr: linux.sockaddr.in = .{ .port = 0, .addr = std.mem.nativeToBig(u32, loopback_v4) };
        if (linux.errno(linux.bind(fd, @ptrCast(&addr), @sizeOf(linux.sockaddr.in))) != .SUCCESS) return error.PortProbeFailed;
        var len: linux.socklen_t = @sizeOf(linux.sockaddr.in);
        if (linux.errno(linux.getsockname(fd, @ptrCast(&addr), &len)) != .SUCCESS) return error.PortProbeFailed;
        try self.port_probes.append(self.gpa, fd);
        return std.mem.bigToNative(u16, addr.port);
    }

    /// This run's meta and data service ports on tcp; 0 (unused) on loop.
    fn servicePorts(self: *Rig, transport: configfs.Trtype) ![2]u16 {
        if (transport == .loop) return .{ 0, 0 };
        return .{ try self.tcpPort(), try self.tcpPort() };
    }

    /// `share --create-size` under the harness NQN; recorded in the manifest.
    fn share(self: *Rig, name: []const u8, size: u64, transport: configfs.Trtype, port: u16) ![]u8 {
        var nb: [256]u8 = undefined;
        const nqn = self.nqnFor(&nb, name);
        var bb: [linux.PATH_MAX]u8 = undefined;
        const backing = try std.fmt.bufPrint(&bb, "{s}/{s}.img", .{ self.root, name });
        var sb: [32]u8 = undefined;
        const size_text = try std.fmt.bufPrint(&sb, "{d}", .{size});
        var pb: [8]u8 = undefined;
        const port_text = try std.fmt.bufPrint(&pb, "{d}", .{port});
        const out = switch (transport) {
            .loop => try self.sqzOk(&.{ "nvmeof", "share", "--subnqn", nqn, "--transport", "loop", "--create-size", size_text, backing }),
            .tcp => try self.sqzOk(&.{ "nvmeof", "share", "--subnqn", nqn, "--ip", "127.0.0.1", "--port", port_text, "--create-size", size_text, backing }),
        };
        defer self.gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "shared ") != null);
        try testing.expect(std.mem.indexOf(u8, out, "resv_enable=1") != null);
        const owned = try self.gpa.dupe(u8, nqn);
        try self.shares.append(self.gpa, owned);
        return owned;
    }

    fn unshare(self: *Rig, nqn: []const u8) !void {
        const out = try self.sqzOk(&.{ "nvmeof", "unshare", nqn });
        defer self.gpa.free(out);
        for (self.shares.items, 0..) |s, i| if (std.mem.eql(u8, s, nqn)) {
            self.gpa.free(self.shares.orderedRemove(i));
            break;
        };
    }

    fn connect(self: *Rig, nqn: []const u8, transport: configfs.Trtype, port: u16, ident: ?initiator.HostPair, extra: []const []const u8) !Connected {
        var env_b: ?std.process.Environ.Map = if (ident) |i| try self.envWith(i) else null;
        defer if (env_b) |*e| e.deinit();
        var argv: [16][]const u8 = undefined;
        var n: usize = 0;
        argv[n] = "nvmeof";
        argv[n + 1] = "connect";
        n += 2;
        var pb: [8]u8 = undefined;
        if (transport == .loop) {
            argv[n] = "--transport";
            argv[n + 1] = "loop";
            n += 2;
        } else {
            argv[n] = "--ip";
            argv[n + 1] = "127.0.0.1";
            argv[n + 2] = "--port";
            argv[n + 3] = try std.fmt.bufPrint(&pb, "{d}", .{port});
            n += 4;
        }
        for (extra) |e| {
            argv[n] = e;
            n += 1;
        }
        argv[n] = nqn;
        n += 1;
        const r = try self.sqzEnv(argv[0..n], if (env_b) |*e| e else &self.env);
        defer self.freeRun(r);
        if (!r.term.success()) {
            std.debug.print("\n--- connect {s} failed ({f}) ---\n{s}\n{s}\n", .{ nqn, r.term, r.stdout, r.stderr });
            return error.ConnectFailed;
        }
        var c: Connected = .{};
        if (ident) |i| {
            @memcpy(c.hostnqn_buf[0..i.hostnqn.len], i.hostnqn);
            c.hostnqn_len = i.hostnqn.len;
            @memcpy(c.hostid_buf[0..i.hostid.len], i.hostid);
            c.hostid_len = i.hostid.len;
        }
        @memcpy(c.subnqn[0..nqn.len], nqn);
        c.subnqn_len = nqn.len;
        const dev = lineAfter(r.stdout, "  device ") orelse return error.ConnectOutput;
        @memcpy(c.device[0..dev.len], dev);
        c.device_len = dev.len;
        const chr = lineAfter(r.stdout, "  char ") orelse return error.ConnectOutput;
        @memcpy(c.char[0..chr.len], chr);
        c.char_len = chr.len;
        c.instance = std.fmt.parseInt(u32, lineAfter(r.stdout, "  instance ") orelse return error.ConnectOutput, 10) catch return error.ConnectOutput;
        try self.connects.append(self.gpa, c);
        return c;
    }

    fn disconnect(self: *Rig, c: *const Connected) !void {
        var env_b: ?std.process.Environ.Map = if (c.ident()) |i| try self.envWith(i) else null;
        defer if (env_b) |*e| e.deinit();
        const r = try self.sqzEnv(&.{ "nvmeof", "disconnect", "--identity-only", c.nqn() }, if (env_b) |*e| e else &self.env);
        defer self.freeRun(r);
        if (!r.term.success()) {
            std.debug.print("\n--- disconnect {s} failed ({f}) ---\n{s}\n", .{ c.nqn(), r.term, r.stderr });
            return error.DisconnectFailed;
        }
        for (self.connects.items, 0..) |cc, i| if (cc.instance == c.instance) {
            _ = self.connects.orderedRemove(i);
            break;
        };
    }

    fn disconnectQuiet(c: *const Connected) void {
        initiator.deleteController(c.instance) catch {};
        _ = initiator.awaitControllerGone(c.instance);
    }

    fn listJson(self: *Rig) ![]u8 {
        return self.sqzOk(&.{ "nvmeof", "list", "--json" });
    }

    /// `config set-fabric-endpoints` for the set's FIRST data volume: the
    /// record that makes the data volume the daemon's own connect.
    fn setDataEndpoint(self: *Rig, meta_uri: []const u8, transport: configfs.Trtype, port: u16, data_nqn: []const u8) !void {
        const show = try self.sqzOk(&.{ "config", "show", "--json", meta_uri });
        defer self.gpa.free(show);
        const tag = json_scan.string(show, "vol_tag") orelse return error.NoVolTag;
        var spec: [400]u8 = undefined;
        const word = switch (transport) {
            .tcp => try std.fmt.bufPrint(&spec, "{s}=tcp:127.0.0.1:{d}:{s}", .{ tag, port, data_nqn }),
            .loop => try std.fmt.bufPrint(&spec, "{s}=loop:::{s}", .{ tag, data_nqn }),
        };
        const out = try self.sqzOk(&.{ "config", "set-fabric-endpoints", meta_uri, word });
        defer self.gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "fabric endpoints set") != null);
    }

    /// The controllers of `nqn` under `ident`, deleted (a daemon's
    /// leftovers after a kill -9 are the rig's to sweep); the count.
    fn disconnectIdentity(self: *Rig, nqn: []const u8, ident: initiator.HostPair) !u32 {
        const n = try initiator.disconnect(self.gpa, .{ .subsysnqn = nqn, .identity = .{ .hostnqn = ident.hostnqn, .hostid = ident.hostid } });
        const deadline = clock.monotonicNs() + initiator.DISCONNECT_WAIT_S * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            if ((try self.controllersOf(nqn, ident)) == 0) break;
            sleepMs(50);
        }
        return n;
    }

    /// Live controllers of `nqn` carrying `ident` (null = any identity).
    fn controllersOf(self: *Rig, nqn: []const u8, ident: ?initiator.HostPair) !u32 {
        const ctrls = try initiator.listControllers(self.gpa, initiator.SYSFS_ROOT);
        defer self.gpa.free(ctrls);
        var n: u32 = 0;
        for (ctrls) |c| {
            if (!std.mem.eql(u8, c.subsysnqnOf(), nqn)) continue;
            if (ident) |i| if (!c.hasIdentity(i.hostnqn, i.hostid)) continue;
            n += 1;
        }
        return n;
    }

    /// `/sys/class/nvme` as a sorted census of `nvme<N>` names, for the
    /// "as it was" comparison around a mount.
    fn controllerCensus(self: *Rig) ![]u8 {
        const ctrls = try initiator.listControllers(self.gpa, initiator.SYSFS_ROOT);
        defer self.gpa.free(ctrls);
        var out: Io.Writer.Allocating = .init(self.gpa);
        errdefer out.deinit();
        for (ctrls) |c| try out.writer.print("{s}:{s}:{s}\n", .{ c.nameOf(), c.subsysnqnOf(), c.hostnqnOf() });
        return out.toOwnedSlice();
    }

    fn resvReportJson(self: *Rig, dev: []const u8) ![]u8 {
        return self.sqzOk(&.{ "nvmeof", "resv-report", "--json", dev });
    }

    /// The ledger record of `nqn`.
    fn ledgerRecord(self: *Rig, nqn: []const u8) !?ledger.Record {
        var store = try ledger.Store.init(self.gpa, self.pool, self.state);
        var l = try store.load();
        defer l.deinit(self.gpa);
        const r = l.find(nqn) orelse return null;
        return try r.clone(self.gpa);
    }

    /// O_DIRECT round trip of `roundtrip_len` random bytes through the
    /// io_uring file layer on the block node; the content read back
    /// equals what was written (hash64 per chunk).
    fn roundTrip(self: *Rig, dev: []const u8) !void {
        _ = uring_fs.registerMetaDevice(self.gpa, &self.env, dev) catch return error.DirectOpenRefused;
        var buf = try uring_fs.AlignedBuf.zeroed(self.gpa, roundtrip_chunk, 4096);
        defer buf.deinit();
        var sums: [roundtrip_len / roundtrip_chunk]u64 = undefined;
        var prng = std.Random.DefaultPrng.init(@intCast(self.pid));
        for (&sums, 0..) |*s, i| {
            prng.random().bytes(buf.bytes);
            s.* = checksum.hash64(buf.bytes);
            try self.pool.writeAt(dev, i * roundtrip_chunk, buf.bytes);
        }
        try self.pool.fdatasync(dev);
        for (sums, 0..) |s, i| {
            var res = try self.pool.readAt(dev, i * roundtrip_chunk, roundtrip_chunk);
            defer res.deinit();
            try testing.expectEqual(s, checksum.hash64(res.bytes()));
        }
    }

    // ---- daemons (foreground mounts) ----

    fn spawnMount(self: *Rig, name: []const u8, meta_uri: []const u8, ident: ?initiator.HostPair, extra: []const []const u8) !usize {
        const mp = try std.fmt.allocPrintSentinel(self.gpa, "{s}/mnt-{s}", .{ self.root, name }, 0);
        errdefer self.gpa.free(mp);
        _ = linux.mkdir(mp.ptr, 0o755);
        const log_path = try std.fmt.allocPrintSentinel(self.gpa, "{s}/{s}.log", .{ self.root, name }, 0);
        errdefer self.gpa.free(log_path);
        const log_file = try Io.Dir.cwd().createFile(self.io, log_path, .{ .truncate = false });
        errdefer log_file.close(self.io);
        var env_b: ?std.process.Environ.Map = if (ident) |i| try self.envWith(i) else null;
        defer if (env_b) |*e| e.deinit();
        var argv: [16][]const u8 = undefined;
        var n: usize = 0;
        argv[n] = self.bin;
        argv[n + 1] = "mount";
        n += 2;
        for (extra) |e| {
            argv[n] = e;
            n += 1;
        }
        argv[n] = meta_uri;
        argv[n + 1] = mp;
        n += 2;
        const child = try std.process.spawn(self.io, .{ .argv = argv[0..n], .environ_map = if (env_b) |*e| e else &self.env, .stdin = .ignore, .stdout = .{ .file = log_file }, .stderr = .{ .file = log_file } });
        try self.daemons.append(self.gpa, .{ .child = child, .pid = child.id.?, .log_path = log_path, .mp = mp, .log_file = log_file });
        return self.daemons.items.len - 1;
    }

    fn readLog(self: *Rig, d: usize) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.io, self.daemons.items[d].log_path, self.gpa, .limited(16 << 20));
    }

    /// Bounded poll of the daemon's log for a needle; a dead daemon fails.
    fn awaitLog(self: *Rig, d: usize, needle: []const u8, deadline_ns: u64) !void {
        const deadline = clock.monotonicNs() + deadline_ns;
        while (true) {
            const text = try self.readLog(d);
            defer self.gpa.free(text);
            if (std.mem.indexOf(u8, text, needle) != null) return;
            if (!alive(self.daemons.items[d].pid)) {
                std.debug.print("\n--- daemon died waiting for '{s}' ---\n{s}\n", .{ needle, text });
                return error.DaemonDied;
            }
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- timed out waiting for '{s}' ---\n{s}\n", .{ needle, text });
                return error.Timeout;
            }
            sleepMs(50);
        }
    }

    /// The mount's `.stats` document.
    fn stats(self: *Rig, d: usize) ![]u8 {
        var pb: [linux.PATH_MAX]u8 = undefined;
        const p = try std.fmt.bufPrint(&pb, "{s}/.stats", .{self.daemons.items[d].mp});
        return Io.Dir.cwd().readFileAlloc(self.io, p, self.gpa, .limited(16 << 20));
    }

    /// Poll `.stats` until `key == want` (bounded).
    fn awaitStat(self: *Rig, d: usize, key: []const u8, want: u64, deadline_ns: u64) ![]u8 {
        const deadline = clock.monotonicNs() + deadline_ns;
        while (true) {
            const text = self.stats(d) catch |e| {
                if (clock.monotonicNs() >= deadline) return e;
                sleepMs(200);
                continue;
            };
            if (json_scan.number(text, key)) |v| if (v == want) return text;
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- {s} never reached {d}: {?d} ---\n", .{ key, want, json_scan.number(text, key) });
                return text;
            }
            self.gpa.free(text);
            sleepMs(500);
        }
    }

    fn kill9(self: *Rig, d: usize) !void {
        const dm = &self.daemons.items[d];
        _ = linux.kill(dm.pid, .KILL);
        _ = try dm.child.wait(self.io);
        dm.reaped = true;
        _ = linux.umount2(dm.mp.ptr, linux.MNT.DETACH);
    }

    fn umount(self: *Rig, d: usize) !void {
        const dm = &self.daemons.items[d];
        const out = try self.sqzOk(&.{ "umount", dm.mp });
        self.gpa.free(out);
        const deadline = clock.monotonicNs() + exit_wait_ns;
        while (alive(dm.pid) and clock.monotonicNs() < deadline) sleepMs(20);
        _ = try dm.child.wait(self.io);
        dm.reaped = true;
    }

    /// `sh -e -c script` with the mount as cwd.
    fn onMount(self: *Rig, d: usize, script: []const u8) ![]u8 {
        const r = try std.process.run(self.gpa, self.io, .{ .argv = &.{ "sh", "-e", "-c", script }, .cwd = .{ .path = self.daemons.items[d].mp }, .environ_map = &self.env });
        defer self.gpa.free(r.stderr);
        if (!r.term.success()) {
            std.debug.print("\n--- mount script failed ({f}): {s}\n{s}\n{s}\n", .{ r.term, script, r.stdout, r.stderr });
            self.gpa.free(r.stdout);
            return error.ScriptFailed;
        }
        return r.stdout;
    }
};

fn lineAfter(text: []const u8, prefix: []const u8) ?[]const u8 {
    const at = std.mem.indexOf(u8, text, prefix) orelse return null;
    const rest = text[at + prefix.len ..];
    const end = std.mem.indexOfScalar(u8, rest, '\n') orelse rest.len;
    return rest[0..end];
}

fn alive(pid: linux.pid_t) bool {
    var pb: [64]u8 = undefined;
    var buf: [512]u8 = undefined;
    const path = std.fmt.bufPrint(&pb, "/proc/{d}/stat", .{pid}) catch return false;
    const text = Io.Dir.cwd().readFile(testing.io, path, &buf) catch return false;
    const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse return false;
    if (close + 2 >= text.len) return false;
    return text[close + 2] != 'Z' and text[close + 2] != 'X';
}

fn sleepMs(ms: u64) void {
    var ts: linux.timespec = .{ .sec = @intCast(ms / 1000), .nsec = @intCast((ms % 1000) * std.time.ns_per_ms) };
    _ = linux.nanosleep(&ts, &ts);
}

/// The venue: root, the binary beside the cwd, `/dev/nvme-fabrics`, the
/// nvmet tree (preflight mounts configfs), `/dev/fuse`.
fn venue(gpa: Allocator, io: Io) !*Rig {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, "squeezefs", 0, .{ .TYPE = true }, &st)) != .SUCCESS) return testkit.skip(@src(), .other_step, "no ./squeezefs in the cwd — the tier runs under `zig build nvmet-smoke`");
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "the nvmet target needs root");
    if (linux.errno(linux.statx(linux.AT.FDCWD, initiator.FABRICS_DEV, 0, .{ .TYPE = true }, &st)) != .SUCCESS) return testkit.skip(@src(), .no_device, initiator.FABRICS_DEV ++ " absent");
    var e: ?linux.E = null;
    if (configfs.preflight(&e) != .ready) return testkit.skip(@src(), .no_kernel_feature, "the kernel nvmet target is not ready (configfs preflight)");
    return Rig.init(gpa, io) catch |err| switch (err) {
        error.RingSetupFailed => testkit.skip(@src(), .no_kernel_feature, "the io_uring file layer's ring did not set up on this host"),
        else => err,
    };
}

/// The zero-residue census for a NQN: no subsystem, no record, no
/// controller of it.
fn expectNoResidue(rig: *Rig, nqn: []const u8) !void {
    var walk = try configfs.walkLive(configfs.RealFs.face(), rig.gpa);
    defer walk.deinit(rig.gpa);
    try testing.expect(walk.find(nqn) == null);
    for (walk.ports.entries) |p| for (p.links) |l| try testing.expect(!std.mem.eql(u8, l, nqn));
    try testing.expect((try rig.ledgerRecord(nqn)) == null);
    const ctrls = try initiator.listControllers(rig.gpa, initiator.SYSFS_ROOT);
    defer rig.gpa.free(ctrls);
    for (ctrls) |c| try testing.expect(!std.mem.eql(u8, c.subsysnqnOf(), nqn));
}

/// A small configfs port id of this run's own for a hand-built foreign
/// share: below the product's and the harness's ranges, keyed by the pid
/// so two runs start apart, the first id no port holds (the probe — a
/// concurrent run's port is skipped, never reused).
fn smallPortId(fs: configfs.Fs, pid: linux.pid_t) !u16 {
    const span: u16 = configfs.PORT_ID_BASE_DEFAULT - 1;
    var id: u16 = 1 + @as(u16, @intCast(@mod(pid, span)));
    var tries: u16 = 0;
    var pb: [32]u8 = undefined;
    while (tries < span) : (tries += 1) {
        if (!fs.exists(try std.fmt.bufPrint(&pb, "ports/{d}", .{id}))) return id;
        id = if (id == span) 1 else id + 1;
    }
    return error.PortRangeExhausted;
}

fn roundtripLeg(transport: configfs.Trtype) !void {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const port = (try rig.servicePorts(transport))[0];
    const nqn = try rig.share("rt", meta_len, transport, port);
    // The live tree: resv_enable 1, the uuid the ledger recorded.
    var walk = try configfs.walkLive(configfs.RealFs.face(), gpa);
    defer walk.deinit(gpa);
    const live = walk.find(nqn).?;
    try testing.expect(live.enabled);
    var rec = (try rig.ledgerRecord(nqn)).?;
    defer rec.deinit(gpa);
    try testing.expectEqual(rec.ns_uuid, live.device_uuid.?);
    try testing.expectEqual(ledger.State.active, rec.state);
    try testing.expect(configfs.isHarnessPortId(rec.listeners[0].port_id));
    var b: [16]u8 = undefined;
    var pb: [512]u8 = undefined;
    try testing.expectEqualStrings("1", try configfs.RealFs.face().readAttr(try std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/1/resv_enable", .{nqn}), &b));
    const lj = try rig.listJson();
    defer gpa.free(lj);
    try testing.expect(std.mem.indexOf(u8, lj, "\"classification\":\"managed\"") != null);
    // Connect under the derived identity; the namespace carries the uuid.
    const c = try rig.connect(nqn, transport, port, null, &.{});
    try testing.expect(std.mem.startsWith(u8, c.dev(), "/dev/nvme"));
    try testing.expect(std.mem.startsWith(u8, c.chr(), "/dev/ng"));
    const ns = (try initiator.findNamespaceUnderController(gpa, initiator.SYSFS_ROOT, c.instance)).?;
    try testing.expectEqual(rec.ns_uuid, ns.uuid.?);
    const lj2 = try rig.listJson();
    defer gpa.free(lj2);
    try testing.expect(std.mem.indexOf(u8, lj2, "\"connected_fabric_disks\":[{") != null);
    try rig.roundTrip(c.dev());
    // The report verb on a fresh namespace: regctl 0, RESCAP ≠ 0.
    const rr = try rig.resvReportJson(c.dev());
    defer gpa.free(rr);
    try testing.expectEqual(@as(?u64, 0), json_scan.number(rr, "regctl"));
    try testing.expect(json_scan.number(rr, "rescap").? != 0);
    try rig.disconnect(&c);
    try rig.unshare(nqn);
    try expectNoResidue(rig, nqn);
    // The port object went last-out (no other link rode it).
    try testing.expect(!configfs.RealFs.face().exists(try std.fmt.bufPrint(&pb, "ports/{d}", .{rec.listeners[0].port_id})));
}

test "nvmet fidelity: roundtrip-loop — share → list managed+live → connect → the namespace carries the ledger's uuid → 64 MiB O_DIRECT round trip → resv-report → disconnect → unshare → zero residue" {
    try roundtripLeg(.loop);
}

test "nvmet fidelity: roundtrip-tcp — the same on nvmet-tcp over 127.0.0.1" {
    try roundtripLeg(.tcp);
}

fn prMatrixLeg(transport: configfs.Trtype) !void {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const port = (try rig.servicePorts(transport))[0];
    const nqn = try rig.share("pr", meta_len, transport, port);
    const a = try rig.connect(nqn, transport, port, null, &.{});
    const b = try rig.connect(nqn, transport, port, ident_b, &.{});
    const out = try rig.sqzOk(&.{ "nvmeof", "resv-report", "--json", "--pr-matrix", "--peer", b.dev(), a.dev() });
    defer gpa.free(out);
    std.debug.print("\npr-matrix ({s}): {s}\n", .{ transport.name(), out });
    try testing.expectEqual(@as(?bool, true), json_scan.boolean(out, "all_ok"));
    // Zero residue after the matrix.
    const rr = try rig.resvReportJson(a.dev());
    defer gpa.free(rr);
    try testing.expectEqual(@as(?u64, 0), json_scan.number(rr, "regctl"));
    if (transport == .tcp) {
        // Registration persists across the holder's DISCONNECT: nvmet keys
        // registrations by host id, not by controller.
        const dev_a = switch (pr_linux.resolve(gpa, a.dev())) {
            .device => |d| d,
            .none => return error.NotAnNvmeNamespace,
        };
        var client = try pr.Client.init(gpa, dev_a.face(), dev_a.nsid, 0xD15C_0000_0000_0001);
        _ = try client.probeRescap();
        _ = try client.registerLadder();
        client.deinit();
        dev_a.deinit();
        try rig.disconnect(&a);
        const a2 = try rig.connect(nqn, transport, port, null, &.{});
        const rr2 = try rig.resvReportJson(a2.dev());
        defer gpa.free(rr2);
        try testing.expectEqual(@as(?u64, 1), json_scan.number(rr2, "regctl"));
        try testing.expect(std.mem.indexOf(u8, rr2, "\"rkey\":\"d15c000000000001\"") != null);
        // Drain it.
        const dev_a2 = switch (pr_linux.resolve(gpa, a2.dev())) {
            .device => |d| d,
            .none => return error.NotAnNvmeNamespace,
        };
        defer dev_a2.deinit();
        var c2 = try pr.Client.init(gpa, dev_a2.face(), dev_a2.nsid, 0xD15C_0000_0000_0001);
        defer c2.deinit();
        try c2.unregister(0xD15C_0000_0000_0001);
        try rig.disconnect(&a2);
    } else {
        try rig.disconnect(&a);
    }
    try rig.disconnect(&b);
    try rig.unshare(nqn);
    try expectNoResidue(rig, nqn);
}

test "nvmet fidelity: pr-matrix-loop — the real reservation legs (RESCAP, EDS-first, idempotent register, the spec-strict own-stale ladder, the two-identity WE fence as -EBADE, WERO rtype 3 with the fencing binding, preempt-and-abort, zero residue, shared identity) on nvmet-loop" {
    try prMatrixLeg(.loop);
}

test "nvmet fidelity: pr-matrix-tcp — the same legs on nvmet-tcp, plus a registration surviving the holder's disconnect" {
    try prMatrixLeg(.tcp);
}

/// A set whose data volume is the DAEMON's connect: meta + data shares,
/// both connected by the operator for `format` (the data node's only use
/// before the record), the data volume's `fabric_endpoint` record set,
/// the operator's data association released — the daemon connects its
/// own at mount.
const DaemonSet = struct {
    meta_nqn: []const u8,
    data_nqn: []const u8,
    cm: Connected,
    meta_uri_buf: [64]u8 = undefined,
    meta_uri_len: usize = 0,

    fn metaUri(self: *const DaemonSet) []const u8 {
        return self.meta_uri_buf[0..self.meta_uri_len];
    }
};

fn daemonSet(rig: *Rig, prefix: []const u8, transport: configfs.Trtype, ports: [2]u16, format_extra: []const []const u8) !DaemonSet {
    var mb: [64]u8 = undefined;
    var db: [64]u8 = undefined;
    const meta_nqn = try rig.share(try std.fmt.bufPrint(&mb, "{s}meta", .{prefix}), meta_len, transport, ports[0]);
    const data_nqn = try rig.share(try std.fmt.bufPrint(&db, "{s}data", .{prefix}), data_len, transport, ports[1]);
    const cm = try rig.connect(meta_nqn, transport, ports[0], null, &.{});
    const cd = try rig.connect(data_nqn, transport, ports[1], null, &.{});
    var out: DaemonSet = .{ .meta_nqn = meta_nqn, .data_nqn = data_nqn, .cm = cm };
    out.meta_uri_len = (try std.fmt.bufPrint(&out.meta_uri_buf, "sqmeta://{s}", .{cm.dev()})).len;
    var du: [64]u8 = undefined;
    const data_uri = try std.fmt.bufPrint(&du, "sqdata://{s}", .{cd.dev()});
    var argv: [12][]const u8 = undefined;
    var n: usize = 0;
    argv[n] = "format";
    argv[n + 1] = "--single-writer";
    n += 2;
    for (format_extra) |e| {
        argv[n] = e;
        n += 1;
    }
    argv[n] = out.metaUri();
    argv[n + 1] = data_uri;
    n += 2;
    const fmt_out = try rig.sqzOk(argv[0..n]);
    defer rig.gpa.free(fmt_out);
    try testing.expect(std.mem.indexOf(u8, fmt_out, "data volumes 1") != null);
    try rig.setDataEndpoint(out.metaUri(), transport, ports[1], data_nqn);
    try rig.disconnect(&cd);
    return out;
}

/// THE row (charter §8): meta + data shares → connect both → format on
/// the connected namespaces → the data volume's endpoint record → mount
/// (the daemon connects the data volume itself) → PR armed → a second
/// identity refused → kill -9 → remount through the own-stale ladder
/// (the data association REUSED) → umount → regctl 0, the association
/// released.
fn mountLeg(transport: configfs.Trtype) !void {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const ports = try rig.servicePorts(transport);
    const set = try daemonSet(rig, "", transport, ports, &.{});
    const meta_nqn = set.meta_nqn;
    const data_nqn = set.data_nqn;
    const cm = set.cm;
    const meta_uri = set.metaUri();
    // Mount A under the rig's identity: the data volume is its own connect.
    const a = try rig.spawnMount("a", meta_uri, null, &.{});
    try rig.awaitLog(a, "ready: ", ready_wait_ns);
    const st = try rig.stats(a);
    defer gpa.free(st);
    try testing.expectEqualStrings("flock+pr", json_scan.string(st, "writer_guard_mode").?);
    try testing.expect(json_scan.number(st, "pr_rescap").? != 0);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st, "pr_rtype"));
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st, "pr_registrants_per_namespace"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st, "pr_own_stale_recoveries"));
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st, "fabric_daemon_connects"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st, "fabric_daemon_reuses"));
    try testing.expect(std.mem.indexOf(u8, st, data_nqn) != null);
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    const pr_key = json_scan.string(st, "pr_key").?;
    // The device agrees: the holder IS the mount's key (through the ng twin
    // of the META connection, under the SAME association).
    const rr = try rig.resvReportJson(cm.dev());
    defer gpa.free(rr);
    try testing.expectEqualStrings(pr_key, json_scan.string(rr, "holder_key").?);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(rr, "regctl"));
    // 8 MiB written and synced; its checksum.
    const sum = try rig.onMount(a, "head -c 8388608 /dev/urandom > payload && sync && cksum payload | cut -d' ' -f1");
    defer gpa.free(sum);
    // A second mount under a SECOND identity pair (its own connect of the
    // meta share): refused — the claim is fresh (the ladder's first rung).
    const cm_b = try rig.connect(meta_nqn, transport, ports[0], ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    const b = try rig.spawnMount("b", meta_uri_b, ident_b, &.{});
    {
        const dm = &rig.daemons.items[b];
        const deadline = clock.monotonicNs() + ready_wait_ns;
        while (alive(dm.pid) and clock.monotonicNs() < deadline) sleepMs(20);
        try testing.expect(!alive(dm.pid));
        const term = try dm.child.wait(rig.io);
        dm.reaped = true;
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, term);
        const text = try rig.readLog(b);
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "single-writer guard") != null);
    }
    try rig.disconnect(&cm_b);
    // kill -9 → remount under the same identity: a DIFFERENT per-incarnation
    // key under the SAME host id — nvmet conflicts, the ladder unregisters
    // exactly the stale one. The data association the killed daemon left
    // (its pair, its subsystem) is REUSED, never a second connect.
    try rig.kill9(a);
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    const a2 = try rig.spawnMount("a2", meta_uri, null, &.{});
    try rig.awaitLog(a2, "ready: ", ready_wait_ns);
    const st2 = try rig.stats(a2);
    defer gpa.free(st2);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st2, "pr_own_stale_recoveries"));
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st2, "pr_registrants_per_namespace"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st2, "writer_guard_fenced"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st2, "fabric_daemon_connects"));
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st2, "fabric_daemon_reuses"));
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    const sum2 = try rig.onMount(a2, "cksum payload | cut -d' ' -f1");
    defer gpa.free(sum2);
    try testing.expectEqualStrings(sum, sum2);
    // umount: release + unregister = zero registrants; the daemon's data
    // association released with it.
    try rig.umount(a2);
    const rr2 = try rig.resvReportJson(cm.dev());
    defer gpa.free(rr2);
    try testing.expectEqual(@as(?u64, 0), json_scan.number(rr2, "regctl"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(rr2, "rtype"));
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(data_nqn, null));
    const log2 = try rig.readLog(a2);
    defer gpa.free(log2);
    try testing.expect(std.mem.indexOf(u8, log2, "under this mount's identity released") != null);
    try rig.disconnect(&cm);
    try rig.unshare(data_nqn);
    try rig.unshare(meta_nqn);
    try expectNoResidue(rig, meta_nqn);
    try expectNoResidue(rig, data_nqn);
}

/// The daemon-owned data-plane connect (the rung-8a3 owed row): a mount
/// by endpoint record with NO pre-connected namespace connects, serves and
/// disconnects leaving `/sys/class/nvme` as it was; without the record the
/// §5.2 ladder refuses the mount naming the verb; a namespace ANOTHER
/// identity connected is never touched by the daemon's release.
fn daemonConnectLeg(transport: configfs.Trtype) !void {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const ports = try rig.servicePorts(transport);
    const meta_nqn = try rig.share("dcmeta", meta_len, transport, ports[0]);
    const data_nqn = try rig.share("dcdata", data_len, transport, ports[1]);
    const cm = try rig.connect(meta_nqn, transport, ports[0], null, &.{});
    const cd = try rig.connect(data_nqn, transport, ports[1], null, &.{});
    var mu: [64]u8 = undefined;
    var du: [64]u8 = undefined;
    const meta_uri = try std.fmt.bufPrint(&mu, "sqmeta://{s}", .{cm.dev()});
    const data_uri = try std.fmt.bufPrint(&du, "sqdata://{s}", .{cd.dev()});
    const fmt_out = try rig.sqzOk(&.{ "format", "--single-writer", meta_uri, data_uri });
    gpa.free(fmt_out);
    // RED: an explicit pair, a data volume with NO record — the ladder
    // refuses UNIFORMLY (the operator's controller under the SAME pair
    // included), naming the verb.
    {
        const r = try rig.spawnMount("dc-norec", meta_uri, null, &.{});
        const dm = &rig.daemons.items[r];
        const deadline = clock.monotonicNs() + ready_wait_ns;
        while (alive(dm.pid) and clock.monotonicNs() < deadline) sleepMs(20);
        try testing.expect(!alive(dm.pid));
        const term = try dm.child.wait(rig.io);
        dm.reaped = true;
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, term);
        const text = try rig.readLog(r);
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "set-fabric-endpoints") != null);
    }
    try rig.setDataEndpoint(meta_uri, transport, ports[1], data_nqn);
    // The operator's data association released: NO pre-connected namespace.
    try rig.disconnect(&cd);
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(data_nqn, null));
    const before = try rig.controllerCensus();
    defer gpa.free(before);
    // The mount connects, serves, and its umount disconnects.
    const a = try rig.spawnMount("dc-a", meta_uri, null, &.{});
    try rig.awaitLog(a, "ready: ", ready_wait_ns);
    const st = try rig.stats(a);
    defer gpa.free(st);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st, "fabric_daemon_connects"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st, "fabric_daemon_reuses"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st, "fabric_ladder_refusals"));
    var needle: [320]u8 = undefined;
    try testing.expect(std.mem.indexOf(u8, st, try std.fmt.bufPrint(&needle, "\"subnqn\":\"{s}\",\"instance\":", .{data_nqn})) != null);
    try testing.expect(std.mem.indexOf(u8, st, "\"reused\":false") != null);
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    const sum = try rig.onMount(a, "head -c 8388608 /dev/urandom > payload && sync && cksum payload | cut -d' ' -f1");
    defer gpa.free(sum);
    const during = try rig.controllerCensus();
    defer gpa.free(during);
    try rig.umount(a);
    const after = try rig.controllerCensus();
    defer gpa.free(after);
    try testing.expectEqualStrings(before, after);
    const log_a = try rig.readLog(a);
    defer gpa.free(log_a);
    try testing.expect(std.mem.indexOf(u8, log_a, "1 controller(s) of ") != null and std.mem.indexOf(u8, log_a, "under this mount's identity released") != null);
    std.debug.print("\ndaemon-connect ({s}): /sys/class/nvme before the mount:\n{s}during (the daemon's own association beside the operator's meta connect):\n{s}after the umount:\n{s}fabric_daemon_connects {?d} fabric_daemon_reuses {?d} fabric_ladder_refusals {?d}\n", .{ transport.name(), before, during, after, json_scan.number(st, "fabric_daemon_connects"), json_scan.number(st, "fabric_daemon_reuses"), json_scan.number(st, "fabric_ladder_refusals") });
    // A namespace ANOTHER process connected (identity B) is never the
    // daemon's to release: the mount connects its OWN beside it and its
    // umount leaves B's standing; the payload reads back through the
    // daemon's fresh association.
    const cd_b = try rig.connect(data_nqn, transport, ports[1], ident_b, &.{});
    const a2 = try rig.spawnMount("dc-a2", meta_uri, null, &.{});
    try rig.awaitLog(a2, "ready: ", ready_wait_ns);
    try testing.expectEqual(@as(u32, 2), try rig.controllersOf(data_nqn, null));
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    const sum2 = try rig.onMount(a2, "cksum payload | cut -d' ' -f1");
    defer gpa.free(sum2);
    try testing.expectEqualStrings(sum, sum2);
    try rig.umount(a2);
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_b));
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(data_nqn, ident_a));
    try rig.disconnect(&cd_b);
    try rig.disconnect(&cm);
    try rig.unshare(data_nqn);
    try rig.unshare(meta_nqn);
    try expectNoResidue(rig, meta_nqn);
    try expectNoResidue(rig, data_nqn);
}

test "nvmet fidelity: daemon-connect-loop — a mount by endpoint record with NO pre-connected namespace connects the data volume itself (fabric_daemon_connects 1, the bound row names the subsystem), serves, and its umount releases exactly its own association (/sys/class/nvme as it was); without the record the ladder refuses naming set-fabric-endpoints; a namespace another identity connected survives the daemon's release" {
    try daemonConnectLeg(.loop);
}

test "nvmet fidelity: daemon-connect-tcp — the same over nvmet-tcp on 127.0.0.1" {
    try daemonConnectLeg(.tcp);
}

test "nvmet fidelity: fidelity-mount (loop) — THE row: the metadata and data volumes ARE connected namespaces (the data volume the daemon's own connect from its endpoint record); format → mount → PR armed for real (holder == pr_key, regctl 1) → a second identity refused → kill -9 → remount recovers via the own-stale ladder (pr_own_stale_recoveries 1, payload intact) and REUSES the data association → umount → regctl 0, the association released → disconnect → unshare" {
    try mountLeg(.loop);
}

test "nvmet fidelity: guard-tcp — the kill -9 remount row on nvmet-tcp" {
    try mountLeg(.tcp);
}

test "nvmet fidelity: sigstop-preempt — a SIGSTOPped holder is preempted by a second identity past T_owner; resumed, its next beat reads the usurpation (writer_guard_fenced 1, the latch FAILED by the fence class) while the second mount's writes land and its own answer EIO" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try daemonSet(rig, "s", transport, try rig.servicePorts(transport), &.{});
    const meta_nqn = set.meta_nqn;
    const data_nqn = set.data_nqn;
    const cm = set.cm;
    const meta_uri = set.metaUri();
    const a = try rig.spawnMount("sa", meta_uri, null, &.{});
    try rig.awaitLog(a, "ready: ", ready_wait_ns);
    const w0 = try rig.onMount(a, "echo before > f && sync");
    gpa.free(w0);
    // Stop A: its claim ages past T_owner (45 s) and its beats stop.
    _ = linux.kill(rig.daemons.items[a].pid, .STOP);
    sleepMs(lease_clock.T_OWNER_MS + 5_000);
    // B under the second identity: a stale claim on a PR substrate — the
    // recovery ladder's PR rung preempts A's key.
    const cm_b = try rig.connect(meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    const b = try rig.spawnMount("sb", meta_uri_b, ident_b, &.{});
    try rig.awaitLog(b, "ready: ", ready_wait_ns);
    const stb = try rig.stats(b);
    defer gpa.free(stb);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(stb, "writer_guard_preempts"));
    const wb = try rig.onMount(b, "echo after > g && sync && cat f");
    defer gpa.free(wb);
    try testing.expectEqualStrings("before\n", wb);
    // Resume A: its next beat finds a foreign holder — fenced; the latch
    // reads failed and B's writes keep landing.
    _ = linux.kill(rig.daemons.items[a].pid, .CONT);
    const sta = try rig.awaitStat(a, "writer_guard_fenced", 1, 30 * std.time.ns_per_s);
    defer gpa.free(sta);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(sta, "writer_guard_fenced"));
    // The latch's verdict: FAILED by the fence class (`fenced`).
    try testing.expectEqualStrings("fenced", json_scan.string(sta, "writer_guard_verdict").?);
    const wb2 = try rig.onMount(b, "echo again > h && sync && cat h");
    defer gpa.free(wb2);
    try testing.expectEqualStrings("again\n", wb2);
    // A's own writes are refused (EIO: the volume is FAILED).
    {
        const r = try std.process.run(gpa, rig.io, .{ .argv = &.{ "sh", "-c", "echo x > z" }, .cwd = .{ .path = rig.daemons.items[a].mp }, .environ_map = &rig.env });
        defer rig.freeRun(r);
        try testing.expect(!r.term.success());
    }
    // B's data association is B's own (identity B beside A's).
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_b));
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    try rig.kill9(a);
    try rig.umount(b);
    // B released exactly its own; the killed A's leftover is the rig's.
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(data_nqn, ident_b));
    try testing.expectEqual(@as(u32, 1), try rig.disconnectIdentity(data_nqn, ident_a));
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(data_nqn);
    try rig.unshare(meta_nqn);
    try expectNoResidue(rig, meta_nqn);
    try expectNoResidue(rig, data_nqn);
}

test "nvmet fidelity: g2-restore-tcp — the PTPL lapse: the subsystem wiped by hand under a LIVE tcp mount; list shows down; restore rebuilds it with the recorded uuid; the controller reconnects; the heartbeat re-acquires (writer_guard_pr_reacquires 1, writer_guard_fenced 0); the payload is intact; a second restore is a verified no-op" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const ports = try rig.servicePorts(.tcp);
    const set = try daemonSet(rig, "g", .tcp, ports, &.{});
    const meta_nqn = set.meta_nqn;
    const data_nqn = set.data_nqn;
    const cm = set.cm;
    const meta_uri = set.metaUri();
    const a = try rig.spawnMount("ga", meta_uri, null, &.{});
    try rig.awaitLog(a, "ready: ", ready_wait_ns);
    const sum = try rig.onMount(a, "head -c 4194304 /dev/urandom > payload && sync && cksum payload | cut -d' ' -f1");
    defer gpa.free(sum);
    var rec = (try rig.ledgerRecord(meta_nqn)).?;
    defer rec.deinit(gpa);
    // The wipe by hand (the target host "rebooted"): the meta subsystem's
    // objects removed through configfs directly — marker-attested, the
    // ledger untouched.
    try testing.expect(configfs.isHarnessNqn(meta_nqn));
    var ids: [ledger.MAX_LISTENERS]u16 = undefined;
    _ = try configfs.teardown(configfs.RealFs.face(), gpa, .{ .subnqn = meta_nqn, .port_ids = rec.portIds(&ids), .allow_hosts = &.{}, .host_named_elsewhere = struct {
        fn f(_: *const anyopaque, _: []const u8) bool {
            return true;
        }
    }.f, .ctx = @ptrCast(&rig) });
    const lj = try rig.listJson();
    defer gpa.free(lj);
    try testing.expect(std.mem.indexOf(u8, lj, "\"classification\":\"down\"") != null);
    const ro = try rig.sqzOk(&.{ "nvmeof", "restore" });
    defer gpa.free(ro);
    try testing.expect(std.mem.indexOf(u8, ro, ": restored") != null);
    var walk = try configfs.walkLive(configfs.RealFs.face(), gpa);
    defer walk.deinit(gpa);
    try testing.expectEqual(rec.ns_uuid, walk.find(meta_nqn).?.device_uuid.?);
    // The host controller reconnects and the daemon's beat re-acquires.
    const sta = try rig.awaitStat(a, "writer_guard_pr_reacquires", 1, g2_wait_ns);
    defer gpa.free(sta);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(sta, "writer_guard_pr_reacquires"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(sta, "writer_guard_fenced"));
    const sum2 = try rig.onMount(a, "cksum payload | cut -d' ' -f1");
    defer gpa.free(sum2);
    try testing.expectEqualStrings(sum, sum2);
    const ro2 = try rig.sqzOk(&.{ "nvmeof", "restore" });
    defer gpa.free(ro2);
    try testing.expect(std.mem.indexOf(u8, ro2, "verified no-op") != null);
    try rig.umount(a);
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(data_nqn, null));
    try rig.disconnect(&cm);
    try rig.unshare(data_nqn);
    try rig.unshare(meta_nqn);
    try expectNoResidue(rig, meta_nqn);
    try expectNoResidue(rig, data_nqn);
}

test "nvmet fidelity: crash-window — W-N1: a share with a TEST-NET-1 listener fails at its second port link (EADDRNOTAVAIL) leaving a PENDING record claiming both ids; a second share of the backing refuses mid-window; restore finalizes; unshare removes both ports. W-N2: a never-linkable share → pending → the subsystem wiped by hand → restore garbage-collects it and sweeps the port shell" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    var nb: [256]u8 = undefined;
    const nqn = try gpa.dupe(u8, rig.nqnFor(&nb, "cw1"));
    try rig.shares.append(gpa, nqn);
    var bb: [linux.PATH_MAX]u8 = undefined;
    const backing = try std.fmt.bufPrint(&bb, "{s}/cw1.img", .{rig.root});
    var p1b: [8]u8 = undefined;
    const p1 = try std.fmt.bufPrint(&p1b, "{d}", .{try rig.tcpPort()});
    const r = try rig.sqz(&.{ "nvmeof", "share", "--subnqn", nqn, "--ip", "127.0.0.1", "--ip", "192.0.2.55", "--port", p1, "--create-size", "67108864", backing });
    defer rig.freeRun(r);
    try testing.expect(!r.term.success());
    try testing.expect(std.mem.indexOf(u8, r.stderr, "PENDING record claims what exists") != null);
    var rec = (try rig.ledgerRecord(nqn)).?;
    defer rec.deinit(gpa);
    try testing.expectEqual(ledger.State.pending, rec.state);
    try testing.expectEqual(@as(usize, 2), rec.listeners.len);
    var ids: [ledger.MAX_LISTENERS]u16 = undefined;
    try testing.expectEqual(@as(usize, 2), rec.portIds(&ids).len);
    const r2 = try rig.sqz(&.{ "nvmeof", "share", "--subnqn", "nqn.2026-10.io.squeezefs:fideli-cw1b", "--transport", "loop", backing });
    defer rig.freeRun(r2);
    try testing.expect(!r2.term.success());
    try testing.expect(std.mem.indexOf(u8, r2.stderr, "PENDING share") != null);
    const ro = try rig.sqzOk(&.{ "nvmeof", "restore" });
    defer gpa.free(ro);
    try testing.expect(std.mem.indexOf(u8, ro, "pending intent finalized") != null);
    const uo = try rig.sqzOk(&.{ "nvmeof", "unshare", nqn });
    defer gpa.free(uo);
    try testing.expect(std.mem.indexOf(u8, uo, "ports removed 2") != null);
    for (rig.shares.items, 0..) |s, i| if (std.mem.eql(u8, s, nqn)) {
        gpa.free(rig.shares.orderedRemove(i));
        break;
    };
    try expectNoResidue(rig, nqn);
    // W-N2.
    const nqn2 = try gpa.dupe(u8, rig.nqnFor(&nb, "cw2"));
    try rig.shares.append(gpa, nqn2);
    const backing2 = try std.fmt.bufPrint(&bb, "{s}/cw2.img", .{rig.root});
    var p2b: [8]u8 = undefined;
    const p2 = try std.fmt.bufPrint(&p2b, "{d}", .{try rig.tcpPort()});
    const r3 = try rig.sqz(&.{ "nvmeof", "share", "--subnqn", nqn2, "--ip", "192.0.2.66", "--port", p2, "--create-size", "67108864", backing2 });
    defer rig.freeRun(r3);
    try testing.expect(!r3.term.success());
    var rec2 = (try rig.ledgerRecord(nqn2)).?;
    defer rec2.deinit(gpa);
    try testing.expectEqual(ledger.State.pending, rec2.state);
    const shell_id = rec2.listeners[0].port_id;
    var pb: [64]u8 = undefined;
    try testing.expect(configfs.RealFs.face().exists(try std.fmt.bufPrint(&pb, "ports/{d}", .{shell_id})));
    // The manual wipe of the subsystem (the namespace is live without a listener).
    _ = try configfs.teardown(configfs.RealFs.face(), gpa, .{ .subnqn = nqn2, .port_ids = &.{}, .allow_hosts = &.{}, .host_named_elsewhere = struct {
        fn f(_: *const anyopaque, _: []const u8) bool {
            return true;
        }
    }.f, .ctx = @ptrCast(&rig) });
    const ro2 = try rig.sqzOk(&.{ "nvmeof", "restore" });
    defer gpa.free(ro2);
    try testing.expect(std.mem.indexOf(u8, ro2, "garbage-collected") != null);
    try testing.expect(!configfs.RealFs.face().exists(try std.fmt.bufPrint(&pb, "ports/{d}", .{shell_id})));
    for (rig.shares.items, 0..) |s, i| if (std.mem.eql(u8, s, nqn2)) {
        gpa.free(rig.shares.orderedRemove(i));
        break;
    };
    try expectNoResidue(rig, nqn2);
}

test "nvmet fidelity: adopt — A1: a hand-built foreign share with a small-int port id is connected, listed foreign, adopted with configfs untouched (the record carries the live uuid and the out-of-range id), re-adopt refuses, unshare removes the small-int port; A2: the ledger file deleted → list foreign → adopt (ledger_loss) → restore no-op; A3: a :fideli- NQN refuses [adopt_harness_owned]" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const fs = configfs.RealFs.face();
    // A1: hand-built through configfs (not the product's verb) on a small
    // port id outside both ranges — the run's own (`smallPortId`).
    var nb: [256]u8 = undefined;
    const hand = try std.fmt.bufPrint(&nb, "nqn.2026-10.example:hand-{d}", .{rig.pid});
    var bb: [linux.PATH_MAX]u8 = undefined;
    const backing = try std.fmt.bufPrint(&bb, "{s}/hand.img", .{rig.root});
    {
        const f = try Io.Dir.cwd().createFile(rig.io, backing, .{ .truncate = true });
        defer f.close(rig.io);
        try f.setLength(rig.io, 64 << 20);
    }
    // A tcp listener: a loop connect reaches only the FIRST loop port on
    // the host, so a hand-built loop port would be unreachable beside any
    // other (a concurrent run's); tcp addresses the port by ip:port.
    const hand_port = try rig.tcpPort();
    var hl = try configfs.Listener.tcp("127.0.0.1", hand_port);
    hl.port_id = try smallPortId(fs, rig.pid);
    var hand_dir_b: [32]u8 = undefined;
    const hand_dir = try std.fmt.bufPrint(&hand_dir_b, "ports/{d}", .{hl.port_id});
    const hand_uuid: [16]u8 = .{ 0x7a, 0x7a, 0x7a, 0x7a, 0x11, 0x11, 0x41, 0x11, 0x81, 0x11, 1, 2, 3, 4, 5, 6 };
    _ = try configfs.apply(fs, .{ .subnqn = hand, .backing_path = backing, .ns_uuid = hand_uuid, .allow_hosts = &.{}, .listeners = &.{hl} });
    const hand_owned = try gpa.dupe(u8, hand);
    try rig.shares.append(gpa, hand_owned);
    const c = try rig.connect(hand, .tcp, hand_port, null, &.{});
    const lj = try rig.listJson();
    defer gpa.free(lj);
    try testing.expect(std.mem.indexOf(u8, lj, "\"foreign_live\":[{\"subnqn\":\"nqn.2026-10.example:hand-") != null);
    var before = try configfs.walkLive(fs, gpa);
    defer before.deinit(gpa);
    const ao = try rig.sqzOk(&.{ "nvmeof", "adopt", hand });
    defer gpa.free(ao);
    try testing.expect(std.mem.indexOf(u8, ao, "adopted ") != null and std.mem.indexOf(u8, ao, "foreign") != null);
    var after = try configfs.walkLive(fs, gpa);
    defer after.deinit(gpa);
    try testing.expectEqual(before.shares.len, after.shares.len);
    try testing.expectEqual(before.ports.entries.len, after.ports.entries.len);
    var rec = (try rig.ledgerRecord(hand)).?;
    defer rec.deinit(gpa);
    try testing.expect(rec.adopted and rec.adopt_class == .foreign);
    try testing.expectEqual(hand_uuid, rec.ns_uuid);
    try testing.expectEqual(hl.port_id, rec.listeners[0].port_id);
    const ra = try rig.sqz(&.{ "nvmeof", "adopt", hand });
    defer rig.freeRun(ra);
    try testing.expect(!ra.term.success() and std.mem.indexOf(u8, ra.stderr, "[adopt_already_ledgered]") != null);
    try rig.roundTrip(c.dev());
    try rig.disconnect(&c);
    try rig.unshare(hand);
    try testing.expect(!fs.exists(hand_dir));
    try expectNoResidue(rig, hand);
    // A2: a product share, the ledger file deleted, adopt as ledger_loss.
    const own = try rig.share("lost", 64 << 20, .loop, 0);
    var lp: [linux.PATH_MAX]u8 = undefined;
    _ = linux.unlink(try std.fmt.bufPrintSentinel(&lp, "{s}/{s}", .{ rig.state, ledger.FILE_NAME }, 0));
    const lj2 = try rig.listJson();
    defer gpa.free(lj2);
    try testing.expect(std.mem.indexOf(u8, lj2, "\"foreign_live\":[{\"subnqn\":\"nqn.2026-10.io.squeezefs:fideli-") != null);
    // A3: the harness marker refuses adoption.
    const r3 = try rig.sqz(&.{ "nvmeof", "adopt", own });
    defer rig.freeRun(r3);
    try testing.expect(!r3.term.success() and std.mem.indexOf(u8, r3.stderr, "[adopt_harness_owned]") != null);
    // The marked share lost its record: the harness tears its objects down
    // by the walk (adopt refused it by design).
    rig.teardownByWalk(own);
    for (rig.shares.items, 0..) |s, i| if (std.mem.eql(u8, s, own)) {
        gpa.free(rig.shares.orderedRemove(i));
        break;
    };
    // The ledger-loss class needs an unmarked product NQN on a tcp port of
    // the PRODUCT range (a loop port would be the harness's shared one and
    // a harness port id refuses adoption): share it with the product base,
    // lose the ledger, adopt, restore, unshare.
    var env_prod = try rig.envWith(ident_a);
    defer env_prod.deinit();
    try env_prod.put(port_base_knob, "53000");
    var nb2: [256]u8 = undefined;
    const plain = try std.fmt.bufPrint(&nb2, "{s}{d}-plain", .{ configfs.OWNERSHIP_NQN_PREFIX, rig.pid });
    const plain_backing = try std.fmt.bufPrint(&bb, "{s}/plain.img", .{rig.root});
    const plain_owned = try gpa.dupe(u8, plain);
    try rig.shares.append(gpa, plain_owned);
    {
        var ppb: [8]u8 = undefined;
        const pp = try std.fmt.bufPrint(&ppb, "{d}", .{try rig.tcpPort()});
        const r = try rig.sqzEnv(&.{ "nvmeof", "share", "--subnqn", plain, "--ip", "127.0.0.1", "--port", pp, "--create-size", "67108864", plain_backing }, &env_prod);
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\n--- plain share failed ---\n{s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }
    _ = linux.unlink(try std.fmt.bufPrintSentinel(&lp, "{s}/{s}", .{ rig.state, ledger.FILE_NAME }, 0));
    const ao2 = try rig.sqzOk(&.{ "nvmeof", "adopt", plain });
    defer gpa.free(ao2);
    try testing.expect(std.mem.indexOf(u8, ao2, "(ledger_loss") != null);
    const ro = try rig.sqzOk(&.{ "nvmeof", "restore" });
    defer gpa.free(ro);
    try testing.expect(std.mem.indexOf(u8, ro, "verified no-op") != null);
    try rig.unshare(plain);
    try expectNoResidue(rig, plain);
    try testing.expect(!fs.exists(try std.fmt.bufPrint(&nb2, "ports/{d}", .{configfs.firstCandidate(53000, "tcp:127.0.0.1:4470")})));
}

test "nvmet fidelity: pr-registrants — 64 loop connects (nr_io_queues 1) under 64 identities each register a key through their /dev/ng node; the product's two-step report reads regctl 64 and 64 + 64 × 64 bytes (past the 4 KiB a fixed read truncates at 63); cap 0; drained to 0" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const n_hosts: u32 = 64;
    const nqn = try rig.share("many", 64 << 20, .loop, 0);
    var conns: [n_hosts]Connected = undefined;
    var i: u32 = 0;
    while (i < n_hosts) : (i += 1) {
        var hb: [128]u8 = undefined;
        var ib: [64]u8 = undefined;
        const hostid = try std.fmt.bufPrint(&ib, "cccccccc-{x:0>4}-4ccc-8ccc-cccccccccccc", .{i});
        const hostnqn = try std.fmt.bufPrint(&hb, "{s}{s}", .{ initiator.HOSTNQN_UUID_PREFIX, hostid });
        conns[i] = try rig.connect(nqn, .loop, 0, .{ .hostnqn = hostnqn, .hostid = hostid }, &.{ "--nr-io-queues", "1" });
        const dev = switch (pr_linux.resolve(gpa, conns[i].dev())) {
            .device => |d| d,
            .none => return error.NotAnNvmeNamespace,
        };
        defer dev.deinit();
        var client = try pr.Client.init(gpa, dev.face(), dev.nsid, 0xC000_0000_0000_0000 | @as(u64, i));
        defer client.deinit();
        _ = try client.probeRescap();
        try testing.expectEqual(pr.LadderOutcome.registered, try client.registerLadder());
    }
    const rr = try rig.resvReportJson(conns[0].dev());
    defer gpa.free(rr);
    try testing.expectEqual(@as(?u64, n_hosts), json_scan.number(rr, "regctl"));
    try testing.expectEqual(@as(?u64, 64 + 64 * n_hosts), json_scan.number(rr, "report_bytes"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(rr, "registrant_cap"));
    // Drain: every registrant unregisters its own key.
    i = 0;
    while (i < n_hosts) : (i += 1) {
        const dev = switch (pr_linux.resolve(gpa, conns[i].dev())) {
            .device => |d| d,
            .none => return error.NotAnNvmeNamespace,
        };
        defer dev.deinit();
        var client = try pr.Client.init(gpa, dev.face(), dev.nsid, 0xC000_0000_0000_0000 | @as(u64, i));
        defer client.deinit();
        try client.unregister(0xC000_0000_0000_0000 | @as(u64, i));
    }
    const rr2 = try rig.resvReportJson(conns[0].dev());
    defer gpa.free(rr2);
    try testing.expectEqual(@as(?u64, 0), json_scan.number(rr2, "regctl"));
    i = 0;
    while (i < n_hosts) : (i += 1) try rig.disconnect(&conns[i]);
    try rig.unshare(nqn);
    try expectNoResidue(rig, nqn);
}

test "nvmet fidelity: loudfail + storage-list — the binary's refusals name their remediation (an unledgered unshare, --nr-io-queues 0 before any side effect, target stop, a missing backing, a '..' host); `storage list` names a connected namespace with its class; `target status` counts" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const r1 = try rig.sqz(&.{ "nvmeof", "unshare", "nqn.2026-10.io.squeezefs:fideli-never" });
    defer rig.freeRun(r1);
    try testing.expect(!r1.term.success() and std.mem.indexOf(u8, r1.stderr, "never tears down target objects it did not record") != null);
    const ctrls_before = try initiator.listControllers(gpa, initiator.SYSFS_ROOT);
    defer gpa.free(ctrls_before);
    const r2 = try rig.sqz(&.{ "nvmeof", "connect", "--transport", "loop", "--nr-io-queues", "0", "nqn.2026-10.io.squeezefs:fideli-never" });
    defer rig.freeRun(r2);
    try testing.expect(!r2.term.success() and std.mem.indexOf(u8, r2.stderr, "refused before any side effect") != null);
    const ctrls_after = try initiator.listControllers(gpa, initiator.SYSFS_ROOT);
    defer gpa.free(ctrls_after);
    try testing.expectEqual(ctrls_before.len, ctrls_after.len);
    const r3 = try rig.sqz(&.{ "nvmeof", "target", "stop" });
    defer rig.freeRun(r3);
    try testing.expect(!r3.term.success() and std.mem.indexOf(u8, r3.stderr, "not a process") != null);
    const r4 = try rig.sqz(&.{ "nvmeof", "share", "--transport", "loop", "/build/fideli-does-not-exist.img" });
    defer rig.freeRun(r4);
    try testing.expect(!r4.term.success() and std.mem.indexOf(u8, r4.stderr, "--create-size") != null);
    var bb: [linux.PATH_MAX]u8 = undefined;
    const backing = try std.fmt.bufPrint(&bb, "{s}/lf.img", .{rig.root});
    const r5 = try rig.sqz(&.{ "nvmeof", "share", "--transport", "loop", "--allow-host", "../x", "--create-size", "1048576", backing });
    defer rig.freeRun(r5);
    try testing.expect(!r5.term.success() and std.mem.indexOf(u8, r5.stderr, "--allow-host") != null);
    // storage list with a connected, formatted share.
    const nqn = try rig.share("st", meta_len, .loop, 0);
    const c = try rig.connect(nqn, .loop, 0, null, &.{});
    var mu: [64]u8 = undefined;
    const meta_uri = try std.fmt.bufPrint(&mu, "sqmeta://{s}", .{c.dev()});
    const fo = try rig.sqzOk(&.{ "format", "--single-writer", meta_uri });
    gpa.free(fo);
    const sl = try rig.sqzOk(&.{ "storage", "list", "--json" });
    defer gpa.free(sl);
    var needle: [128]u8 = undefined;
    try testing.expect(std.mem.indexOf(u8, sl, try std.fmt.bufPrint(&needle, "\"device\":\"{s}\"", .{c.dev()})) != null);
    try testing.expect(std.mem.indexOf(u8, sl, "\"class\":\"meta\"") != null);
    const sv = try rig.sqzOk(&.{ "storage", "validate", c.dev() });
    defer gpa.free(sv);
    try testing.expect(std.mem.indexOf(u8, sv, "squeezefs metadata member") != null and std.mem.indexOf(u8, sv, "RESCAP 0x") != null);
    const ts = try rig.sqzOk(&.{ "nvmeof", "target", "status", "--json" });
    defer gpa.free(ts);
    try testing.expect(json_scan.number(ts, "resv_enabled_namespaces").? >= 1);
    try rig.disconnect(&c);
    try rig.unshare(nqn);
    try expectNoResidue(rig, nqn);
}
