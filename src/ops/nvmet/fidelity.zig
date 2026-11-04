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
//! remount adopts the host's standing registration; `umount` → zero
//! registrants), `co-located mounts` and `stale sibling` (one host
//! identity per host, decision 0003 §DF), `sigstop-preempt` (a stopped holder is preempted past
//! `T_owner`; resumed, its next beat reads the usurpation and its barrier
//! answers the device's `-EBADE` — FAILED), the member-death rows (the
//! manager's fencing registrant preempts the dead member's host, decision
//! 0003 §DG; a frozen member's released windows refused), `g2-restore-tcp` (the PTPL
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
const multipath = @import("multipath.zig");
const fencing = @import("../../meta/sym/membership/fencing.zig");
const proc = @import("../../meta/guard/proc.zig");

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
        // The host registries under the run's root, never the host's /run
        // (review minor 3 of decision 0003 §DJ): every process of the run
        // shares them; the root goes at the teardown.
        var hosts_buf: [linux.PATH_MAX]u8 = undefined;
        try self.env.put("SQUEEZEFS_TEST_HOST_REGISTRY_DIR", try std.fmt.bufPrint(&hosts_buf, "{s}/hosts", .{self.root}));
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
            // Every controller of a harness subsystem is the harness's own
            // (a killed daemon's associations included): deleted before
            // the share goes, so a failed leg leaves no controller behind.
            const ctrls = initiator.listControllers(self.gpa, initiator.SYSFS_ROOT) catch &.{};
            defer self.gpa.free(ctrls);
            for (ctrls) |c| if (std.mem.eql(u8, c.subsysnqnOf(), s)) {
                initiator.deleteController(c.instance) catch {};
                _ = initiator.awaitControllerGone(c.instance);
            };
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
        var e = try self.envWithKnob(hostnqn_knob, ident.hostnqn);
        errdefer e.deinit();
        try e.put(hostid_knob, ident.hostid);
        return e;
    }

    /// The rig's env plus one knob row.
    fn envWithKnob(self: *Rig, key: []const u8, value: []const u8) !std.process.Environ.Map {
        var e = std.process.Environ.Map.init(self.gpa);
        errdefer e.deinit();
        var it = self.env.iterator();
        while (it.next()) |kv| try e.put(kv.key_ptr.*, kv.value_ptr.*);
        try e.put(key, value);
        return e;
    }

    fn nqnFor(self: *Rig, buf: *[256]u8, name: []const u8) []const u8 {
        return std.fmt.bufPrint(buf, "nqn.2026-10.io.squeezefs:fideli-{d}-{s}", .{ self.pid, name }) catch unreachable;
    }

    /// A subsystem this run made (its harness NQNs and its hand-built
    /// one): the host-global censuses judge only these — a concurrent
    /// run's objects come and go beside them.
    fn ownsNqn(self: *Rig, nqn: []const u8) bool {
        var b: [64]u8 = undefined;
        const harness = std.fmt.bufPrint(&b, "nqn.2026-10.io.squeezefs:fideli-{d}-", .{self.pid}) catch unreachable;
        var hb: [64]u8 = undefined;
        const hand = std.fmt.bufPrint(&hb, "nqn.2026-10.example:hand-{d}", .{self.pid}) catch unreachable;
        return std.mem.startsWith(u8, nqn, harness) or std.mem.eql(u8, nqn, hand);
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
        return self.connectAt(nqn, transport, "127.0.0.1", port, ident, extra);
    }

    /// `nvmeof connect` to one listener address (a second path of a share
    /// connected by hand — the metadata volume's multipath).
    fn connectAt(self: *Rig, nqn: []const u8, transport: configfs.Trtype, ip: []const u8, port: u16, ident: ?initiator.HostPair, extra: []const []const u8) !Connected {
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
            argv[n + 1] = ip;
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
        if (lineAfter(r.stdout, "  timeouts ") == null) return error.ConnectOutput;
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
        for (ctrls) |c| {
            if (!self.ownsNqn(c.subsysnqnOf())) continue;
            try out.writer.print("{s}:{s}:{s}\n", .{ c.nameOf(), c.subsysnqnOf(), c.hostnqnOf() });
        }
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
        var env_b: ?std.process.Environ.Map = if (ident) |i| try self.envWith(i) else null;
        defer if (env_b) |*e| e.deinit();
        return self.spawnMountEnv(name, meta_uri, if (env_b) |*e| e else &self.env, extra);
    }

    /// A foreground mount under an explicit environment (a knob row set
    /// beside the rig's identity).
    fn spawnMountEnv(self: *Rig, name: []const u8, meta_uri: []const u8, env: *const std.process.Environ.Map, extra: []const []const u8) !usize {
        const mp = try std.fmt.allocPrintSentinel(self.gpa, "{s}/mnt-{s}", .{ self.root, name }, 0);
        errdefer self.gpa.free(mp);
        _ = linux.mkdir(mp.ptr, 0o755);
        const log_path = try std.fmt.allocPrintSentinel(self.gpa, "{s}/{s}.log", .{ self.root, name }, 0);
        errdefer self.gpa.free(log_path);
        const log_file = try Io.Dir.cwd().createFile(self.io, log_path, .{ .truncate = false });
        errdefer log_file.close(self.io);
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
        const child = try std.process.spawn(self.io, .{ .argv = argv[0..n], .environ_map = env, .stdin = .ignore, .stdout = .{ .file = log_file }, .stderr = .{ .file = log_file } });
        try self.daemons.append(self.gpa, .{ .child = child, .pid = child.id.?, .log_path = log_path, .mp = mp, .log_file = log_file });
        return self.daemons.items.len - 1;
    }

    fn readLog(self: *Rig, d: usize) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.io, self.daemons.items[d].log_path, self.gpa, .limited(16 << 20));
    }

    /// The daemon's last `n` log lines (a miss's diagnosis).
    fn printLogTail(self: *Rig, d: usize, n: usize) void {
        const text = self.readLog(d) catch return;
        defer self.gpa.free(text);
        var start = text.len;
        var lines: usize = 0;
        while (start > 0 and lines <= n) : (start -= 1) {
            if (text[start - 1] == '\n') lines += 1;
        }
        std.debug.print("--- the last {d} lines of {s}'s log ---\n{s}\n", .{ n, self.daemons.items[d].mp, text[start..] });
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
                self.printLogTail(d, 60);
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
/// After our last link left, the port is gone — or it carries only links
/// another run made (an older binary's loop share joins any loop port; two
/// runs racing for one free id share it).
fn expectPortLastOut(rig: *Rig, port_id: u16) !void {
    var walk = try configfs.walkLive(configfs.RealFs.face(), rig.gpa);
    defer walk.deinit(rig.gpa);
    const p = walk.ports.find(port_id) orelse return;
    try testing.expect(p.links.len > 0);
    for (p.links) |l| try testing.expect(!rig.ownsNqn(l));
}

/// `"subnqn":"<nqn>"` inside the list's `foreign_live` array (its entries
/// carry arrays of their own: the array ends where the next key begins).
fn foreignLiveNames(lj: []const u8, nqn: []const u8) bool {
    const at = std.mem.indexOf(u8, lj, "\"foreign_live\":[") orelse return false;
    const end = std.mem.indexOfPos(u8, lj, at, "],\"connected_fabric_disks\":") orelse lj.len;
    var nb: [320]u8 = undefined;
    const needle = std.fmt.bufPrint(&nb, "\"subnqn\":\"{s}\"", .{nqn}) catch return false;
    return std.mem.indexOf(u8, lj[at..end], needle) != null;
}

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
    const ns = (try initiator.controllerNamespace(gpa, initiator.SYSFS_ROOT, c.instance)).?;
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
    // The port object went last-out unless another run's link rides it.
    try expectPortLastOut(rig, rec.listeners[0].port_id);
}

test "nvmet fidelity: roundtrip-loop — share → list managed+live → connect → the namespace carries the ledger's uuid → 64 MiB O_DIRECT round trip → resv-report → disconnect → unshare → zero residue" {
    try roundtripLeg(.loop);
}

test "nvmet fidelity: roundtrip-tcp — the same on nvmet-tcp over 127.0.0.1" {
    try roundtripLeg(.tcp);
}

/// A loop port built by hand the way an operator builds one: `addr_trtype
/// = loop`, no address, a subsystem of its own linked (so it is enabled),
/// on the run's own small id below both ranges. Removed by `deinit`
/// whatever state a failed row left it in.
const ForeignLoopPort = struct {
    nqn_buf: [64]u8 = undefined,
    nqn_len: usize = 0,
    id: u16,

    fn nqn(self: *const ForeignLoopPort) []const u8 {
        return self.nqn_buf[0..self.nqn_len];
    }

    fn stand(rig: *Rig) !ForeignLoopPort {
        const fs = configfs.RealFs.face();
        var self: ForeignLoopPort = .{ .id = try smallPortId(fs, rig.pid) };
        self.nqn_len = (try std.fmt.bufPrint(&self.nqn_buf, "nqn.2026-10.example:loopfg-{d}", .{rig.pid})).len;
        errdefer self.deinit();
        var pb: [512]u8 = undefined;
        var tb: [128]u8 = undefined;
        try fs.mkdir(try std.fmt.bufPrint(&pb, "subsystems/{s}", .{self.nqn()}));
        try fs.mkdir(try std.fmt.bufPrint(&pb, "ports/{d}", .{self.id}));
        try fs.writeAttr(try std.fmt.bufPrint(&pb, "ports/{d}/addr_trtype", .{self.id}), "loop");
        try fs.symlink(try std.fmt.bufPrint(&tb, "subsystems/{s}", .{self.nqn()}), try std.fmt.bufPrint(&pb, "ports/{d}/subsystems/{s}", .{ self.id, self.nqn() }));
        return self;
    }

    /// The port's links as the walk read them (sorted).
    fn links(self: *const ForeignLoopPort, walk: *const configfs.Walk) ![]const []u8 {
        const p = walk.ports.find(self.id) orelse return error.ForeignPortGone;
        return p.links;
    }

    fn deinit(self: *const ForeignLoopPort) void {
        const fs = configfs.RealFs.face();
        var pb: [512]u8 = undefined;
        fs.unlink(std.fmt.bufPrint(&pb, "ports/{d}/subsystems/{s}", .{ self.id, self.nqn() }) catch unreachable) catch {};
        fs.rmdir(std.fmt.bufPrint(&pb, "subsystems/{s}", .{self.nqn()}) catch unreachable) catch {};
        // A share that joined the port still links it: its own teardown
        // removes the port last-out.
        fs.rmdir(std.fmt.bufPrint(&pb, "ports/{d}", .{self.id}) catch unreachable) catch {};
    }
};

test "nvmet fidelity: roundtrip-loop beside a foreign loop port — an operator's enabled loop port (no address, a subsystem of its own, an id below both ranges) stands before the share; the share takes a harness port of its own, never one that existed before it; the connect reaches it and the namespace carries the ledger's uuid; unshare leaves the foreign port and its one link as they were" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const fs = configfs.RealFs.face();
    const foreign = try ForeignLoopPort.stand(rig);
    defer foreign.deinit();
    var before = try configfs.snapshotPorts(fs, gpa);
    defer before.deinit(gpa);
    const nqn = try rig.share("lf", meta_len, .loop, 0);
    var rec = (try rig.ledgerRecord(nqn)).?;
    defer rec.deinit(gpa);
    const port_id = rec.listeners[0].port_id;
    try testing.expect(configfs.isHarnessPortId(port_id));
    try testing.expect(before.find(port_id) == null);
    {
        var walk = try configfs.walkLive(fs, gpa);
        defer walk.deinit(gpa);
        const fl = try foreign.links(&walk);
        try testing.expectEqual(@as(usize, 1), fl.len);
        try testing.expectEqualStrings(foreign.nqn(), fl[0]);
    }
    const c = try rig.connect(nqn, .loop, 0, null, &.{});
    const ns = (try initiator.controllerNamespace(gpa, initiator.SYSFS_ROOT, c.instance)).?;
    try testing.expectEqual(rec.ns_uuid, ns.uuid.?);
    try rig.disconnect(&c);
    try rig.unshare(nqn);
    try expectNoResidue(rig, nqn);
    try expectPortLastOut(rig, port_id);
    var walk = try configfs.walkLive(fs, gpa);
    defer walk.deinit(gpa);
    const fl = try foreign.links(&walk);
    try testing.expectEqual(@as(usize, 1), fl.len);
    try testing.expectEqualStrings(foreign.nqn(), fl[0]);
    var b: [64]u8 = undefined;
    var pb: [64]u8 = undefined;
    try testing.expectEqualStrings("", try fs.readAttr(try std.fmt.bufPrint(&pb, "ports/{d}/addr_traddr", .{foreign.id}), &b));
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

test "nvmet fidelity: pr-matrix-loop — the real reservation legs (RESCAP, EDS-first, idempotent register, the spec-strict own-stale ladder, the two-identity WE fence as -EBADE, WERO rtype 3 with the fencing binding, the holder's preempt of a registrant's key (a no-op on nvmet 7.3 under WERO, spec under rtype 5), preempt-and-abort, zero residue, shared identity) on nvmet-loop" {
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
/// A PR client on a connected namespace's char twin (the caller frees both).
fn clientOn(gpa: Allocator, c: *const Connected, key: u64) !struct { dev: *pr_linux.Device, client: *pr.Client } {
    const dev = switch (pr_linux.resolve(gpa, c.dev())) {
        .device => |d| d,
        .none => return error.NotAnNvmeNamespace,
    };
    errdefer dev.deinit();
    const client = try gpa.create(pr.Client);
    errdefer gpa.destroy(client);
    client.* = try pr.Client.init(gpa, dev.face(), dev.nsid, key);
    client.rtype = nvme.RTYPE_WERO;
    _ = try client.probeRescap();
    return .{ .dev = dev, .client = client };
}

const ident_fa: initiator.HostPair = .{ .hostnqn = "nqn.2014-08.org.nvmexpress:uuid:fafafafa-fafa-4afa-8afa-fafafafafafa", .hostid = "fafafafa-fafa-4afa-8afa-fafafafafafa" };

test "nvmet fidelity: the fencing registrant's device laws on nvmet 7.3 (decision 0003 §DG) — four identities on one loop namespace: host A holds WERO under key K, host A's fencer F registered under the SAME key K from another host id, member host B under KB, successor host C under KC; F's preempt of KB unregisters B (its write refused, -EBADE) and keeps A's reservation — A the holder, F not; C's takeover preempt of K unregisters A AND F in one command; F's preempt then answers a conflict (nothing fenced, C's hold intact)" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const nqn = try rig.share("fl", meta_len, .loop, 0);
    const ca = try rig.connect(nqn, .loop, 0, null, &.{});
    const cf = try rig.connect(nqn, .loop, 0, ident_fa, &.{});
    const cb = try rig.connect(nqn, .loop, 0, ident_b, &.{});
    const cc = try rig.connect(nqn, .loop, 0, ident_c, &.{});
    const key_k: u64 = 0xFE4C_0000_0000_00A1;
    const key_b: u64 = 0xFE4C_0000_0000_00B1;
    const key_c: u64 = 0xFE4C_0000_0000_00C1;
    const a = try clientOn(gpa, &ca, key_k);
    defer {
        a.client.deinit();
        gpa.destroy(a.client);
        a.dev.deinit();
    }
    const f = try clientOn(gpa, &cf, key_k);
    defer {
        f.client.deinit();
        gpa.destroy(f.client);
        f.dev.deinit();
    }
    const b = try clientOn(gpa, &cb, key_b);
    defer {
        b.client.deinit();
        gpa.destroy(b.client);
        b.dev.deinit();
    }
    const c = try clientOn(gpa, &cc, key_c);
    defer {
        c.client.deinit();
        gpa.destroy(c.client);
        c.dev.deinit();
    }
    try fencing.acquireManagerHold(a.client);
    _ = try f.client.registerLadder();
    _ = try b.client.registerLadder();
    _ = uring_fs.registerMetaDevice(gpa, &rig.env, cb.dev()) catch return error.DirectOpenRefused;
    var buf = try uring_fs.AlignedBuf.zeroed(gpa, 4096, 4096);
    defer buf.deinit();
    try rig.pool.writeAt(cb.dev(), 0, buf.bytes);
    // F — a non-holder under the holder's key — preempts B's key.
    try testing.expectEqual(fencing.PreemptOutcome.fenced, fencing.preemptDeadRegistrant(a.client, f.client, key_b));
    {
        var rep = try a.client.report();
        defer rep.deinit(gpa);
        try testing.expectEqual(nvme.RTYPE_WERO, rep.rtype);
        try testing.expect(!rep.registered(key_b));
        try testing.expectEqual(@as(?u64, key_k), rep.holder());
        try testing.expectEqual(@as(usize, 2), rep.registrants.len);
        var holders: u32 = 0;
        for (rep.registrants) |r| {
            if (!r.holds) continue;
            holders += 1;
            try testing.expectEqualSlices(u8, &(proc.parseUuid(ident_a.hostid) orelse return error.BadUuid), &r.host_id);
        }
        try testing.expectEqual(@as(u32, 1), holders);
    }
    try testing.expectError(error.ReservationConflict, rig.pool.writeAt(cb.dev(), 0, buf.bytes));
    // C takes over by preempting the holder's key: A and its fencer go.
    _ = try c.client.registerLadder();
    try c.client.preempt(key_k);
    {
        var rep = try c.client.report();
        defer rep.deinit(gpa);
        try testing.expectEqual(@as(?u64, key_c), rep.holder());
        try testing.expect(!rep.registered(key_k));
    }
    // A, superseded, finishing a recovery: the report shows C holding — the
    // preempt withheld (review M1); the fencer's own command conflicts at
    // the device anyway.
    _ = try b.client.registerLadder();
    try testing.expectEqual(fencing.PreemptOutcome.failed, fencing.preemptDeadRegistrant(a.client, f.client, key_b));
    try testing.expectError(error.ReservationConflict, f.client.preemptAndAbort(key_b));
    {
        var rep = try c.client.report();
        defer rep.deinit(gpa);
        try testing.expectEqual(@as(?u64, key_c), rep.holder());
        try testing.expect(rep.registered(key_b));
    }
    // Zero residue.
    try c.client.release();
    try b.client.unregister(key_b);
    {
        const rr = try rig.resvReportJson(ca.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, 0), json_scan.number(rr, "regctl"));
    }
    try rig.disconnect(&cc);
    try rig.disconnect(&cb);
    try rig.disconnect(&cf);
    try rig.disconnect(&ca);
    try rig.unshare(nqn);
    try expectNoResidue(rig, nqn);
}

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
    return daemonSetFormat(rig, prefix, transport, ports, &.{"--single-writer"}, format_extra);
}

/// The same set, formatted SYMMETRIC (the default format): every later
/// mount of it joins the manager.
fn symmetricSet(rig: *Rig, prefix: []const u8, transport: configfs.Trtype, ports: [2]u16) !DaemonSet {
    return daemonSetFormat(rig, prefix, transport, ports, &.{}, &.{});
}

fn daemonSetFormat(rig: *Rig, prefix: []const u8, transport: configfs.Trtype, ports: [2]u16, mode: []const []const u8, format_extra: []const []const u8) !DaemonSet {
    return daemonSetSized(rig, prefix, transport, ports, mode, format_extra, data_len);
}

fn daemonSetSized(rig: *Rig, prefix: []const u8, transport: configfs.Trtype, ports: [2]u16, mode: []const []const u8, format_extra: []const []const u8, data_size: u64) !DaemonSet {
    var mb: [64]u8 = undefined;
    var db: [64]u8 = undefined;
    const meta_nqn = try rig.share(try std.fmt.bufPrint(&mb, "{s}meta", .{prefix}), meta_len, transport, ports[0]);
    const data_nqn = try rig.share(try std.fmt.bufPrint(&db, "{s}data", .{prefix}), data_size, transport, ports[1]);
    const cm = try rig.connect(meta_nqn, transport, ports[0], null, &.{});
    const cd = try rig.connect(data_nqn, transport, ports[1], null, &.{});
    var out: DaemonSet = .{ .meta_nqn = meta_nqn, .data_nqn = data_nqn, .cm = cm };
    out.meta_uri_len = (try std.fmt.bufPrint(&out.meta_uri_buf, "sqmeta://{s}", .{cm.dev()})).len;
    var du: [64]u8 = undefined;
    const data_uri = try std.fmt.bufPrint(&du, "sqdata://{s}", .{cd.dev()});
    var argv: [12][]const u8 = undefined;
    var n: usize = 0;
    argv[n] = "format";
    n += 1;
    for (mode) |e| {
        argv[n] = e;
        n += 1;
    }
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
/// identity refused → kill -9 → remount under the same host key, the
/// host's registration adopted (the data association REUSED) → umount →
/// the inherited hold stays (regctl 1), the association released.
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
    // 8 MiB written and synced by the file (`sync -d`: the kill below needs
    // a durability point, and syncfs reaches no daemon on a stock 7.3 —
    // decision 0003 §DB.3); its checksum.
    const sum = try rig.onMount(a, "head -c 8388608 /dev/urandom > payload && sync -d payload && cksum payload | cut -d' ' -f1");
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
    // kill -9 → remount under the same identity: the SAME host key (one per
    // host identity and set, decision 0003 §DF) — the host is still
    // registered, so the remount adopts it (an idempotent register, no
    // own-stale recovery) and takes the reservation over. The data
    // association the killed daemon left (its pair, its subsystem) is
    // REUSED, never a second connect.
    try rig.kill9(a);
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    const a2 = try rig.spawnMount("a2", meta_uri, null, &.{});
    try rig.awaitLog(a2, "ready: ", ready_wait_ns);
    const st2 = try rig.stats(a2);
    defer gpa.free(st2);
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st2, "pr_own_stale_recoveries"));
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st2, "pr_host_adoptions"));
    try testing.expectEqualStrings(pr_key, json_scan.string(st2, "pr_key").?);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st2, "pr_registrants_per_namespace"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st2, "writer_guard_fenced"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st2, "fabric_daemon_connects"));
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st2, "fabric_daemon_reuses"));
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    const sum2 = try rig.onMount(a2, "cksum payload | cut -d' ' -f1");
    defer gpa.free(sum2);
    try testing.expectEqualStrings(sum, sum2);
    // umount: the hold a2 INHERITED from the killed incarnation stays (one
    // registration, Write Exclusive): a hold this incarnation did not
    // acquire on an unreserved namespace may carry a predecessor's fence —
    // released, it would re-admit every host that preempt fenced (review 4
    // blocker 2); the next mount adopts or preempts it. The daemon's data
    // association released with it.
    try rig.umount(a2);
    const rr2 = try rig.resvReportJson(cm.dev());
    defer gpa.free(rr2);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(rr2, "regctl"));
    try testing.expectEqual(@as(?u64, nvme.RTYPE_WRITE_EXCLUSIVE), json_scan.number(rr2, "rtype"));
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
    try testing.expect(std.mem.indexOf(u8, st, try std.fmt.bufPrint(&needle, "\"subnqn\":\"{s}\",\"device\":", .{data_nqn})) != null);
    try testing.expect(std.mem.indexOf(u8, st, "\"reused\":false") != null);
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(data_nqn, ident_a));
    const sum = try rig.onMount(a, "head -c 8388608 /dev/urandom > payload && sync -f . && cksum payload | cut -d' ' -f1");
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

/// `share --ip A --ip B …` of a RAM-backed file (`/build/tmp` is the
/// venue's RAM-backed scratch on a ≥ 64 GiB host) under the harness NQN;
/// recorded in the manifest, the backing removed with the rig.
fn shareMulti(rig: *Rig, name: []const u8, size: u64, ips: []const []const u8, port: u16, backing: []const u8) ![]u8 {
    var nb: [256]u8 = undefined;
    const nqn = rig.nqnFor(&nb, name);
    var sb: [32]u8 = undefined;
    const size_text = try std.fmt.bufPrint(&sb, "{d}", .{size});
    var pb: [8]u8 = undefined;
    const port_text = try std.fmt.bufPrint(&pb, "{d}", .{port});
    var argv: [24][]const u8 = undefined;
    var n: usize = 0;
    for ([_][]const u8{ "nvmeof", "share", "--subnqn", nqn }) |a| {
        argv[n] = a;
        n += 1;
    }
    for (ips) |ip| {
        argv[n] = "--ip";
        argv[n + 1] = ip;
        n += 2;
    }
    for ([_][]const u8{ "--port", port_text, "--create-size", size_text, backing }) |a| {
        argv[n] = a;
        n += 1;
    }
    const out = try rig.sqzOk(argv[0..n]);
    defer rig.gpa.free(out);
    try testing.expect(std.mem.indexOf(u8, out, "shared ") != null);
    for (ips) |ip| {
        var needle: [64]u8 = undefined;
        try testing.expect(std.mem.indexOf(u8, out, try std.fmt.bufPrint(&needle, "listener tcp {s}:{d}", .{ ip, port })) != null);
    }
    const owned = try rig.gpa.dupe(u8, nqn);
    try rig.shares.append(rig.gpa, owned);
    return owned;
}

/// The instances of `nqn`'s controllers under `ident`, ascending.
fn instancesOf(rig: *Rig, nqn: []const u8, ident: initiator.HostPair, out: *[16]u32) ![]const u32 {
    const ctrls = try initiator.listControllers(rig.gpa, initiator.SYSFS_ROOT);
    defer rig.gpa.free(ctrls);
    var n: usize = 0;
    for (ctrls) |c| {
        if (!std.mem.eql(u8, c.subsysnqnOf(), nqn) or !c.hasIdentity(ident.hostnqn, ident.hostid) or n == out.len) continue;
        out[n] = c.instance;
        n += 1;
    }
    std.mem.sort(u32, out[0..n], {}, std.sort.asc(u32));
    return out[0..n];
}

/// The per-path block stats of `instances`, each path's hidden node
/// awaited (the kernel's scan of a later path is asynchronous).
fn pathStats(rig: *Rig, instances: []const u32, nodes: *[16][32]u8, out: *[16]multipath.BlockStat) !void {
    const deadline = clock.monotonicNs() + ready_wait_ns;
    for (instances, 0..) |inst, i| {
        nodes[i] = @splat(0);
        while (true) {
            if (try multipath.pathNode(rig.gpa, initiator.SYSFS_ROOT, inst, &nodes[i])) |node| {
                if (multipath.readNodeStat(initiator.SYSFS_ROOT, inst, node)) |st| {
                    out[i] = st;
                    break;
                }
            }
            if (clock.monotonicNs() >= deadline) return error.PathNodeTimeout;
            sleepMs(50);
        }
    }
}

test "nvmet fidelity: multipath-tcp — RAM-backed metadata and data shares with TWO tcp listeners each (127.0.0.1 and 127.0.0.2); the metadata head connected by hand on both paths under the one pair (rule 2 per path accepts, fabric_meta_bound lists both live, the policy set on it too); the data endpoint record naming both paths plus a DEAD third (127.0.0.3, no listener): the daemon connects both under its one identity (fabric_daemon_connects 2, fabric_paths_degraded 1, fabric_paths_live 2) and ONE head serves them (one nvme-subsys, both hidden path nodes under it); the DEFAULT iopolicy (round-robin) written and read back on both subsystems → a parallel write workload lands sectors on BOTH paths; kill -9 → remount under an explicit queue-depth REUSES both paths (fabric_daemon_reuses 2), flips both subsystems to it and its cold serial read-back is measured per path; umount releases both → a mount under the default again reconnects both, reads round-robin back and four parallel cold readers land read sectors on BOTH; `nvmeof status` names the subsystem, its policy and every path; zero residue" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    if (multipath.readMultipathParam() != .on) return testkit.skip(@src(), .no_kernel_feature, "nvme_core.multipath is not Y on this kernel (no native multipath head)");
    const ports = try rig.servicePorts(.tcp);
    var bb: [linux.PATH_MAX]u8 = undefined;
    var mbb: [linux.PATH_MAX]u8 = undefined;
    const meta_backing = try std.fmt.bufPrintSentinel(&mbb, "/build/tmp/fideli-mpath-meta-{d}.img", .{rig.pid}, 0);
    defer _ = linux.unlink(meta_backing.ptr);
    const backing = try std.fmt.bufPrintSentinel(&bb, "/build/tmp/fideli-mpath-{d}.img", .{rig.pid}, 0);
    defer _ = linux.unlink(backing.ptr);
    // The METADATA share too has two listeners: the operator connects both
    // paths by hand under the one pair (the product's own initiator, the
    // timeouts it derives) and the mount meets a head.
    const meta_nqn = try shareMulti(rig, "mpmeta", meta_len, &.{ "127.0.0.1", "127.0.0.2" }, ports[0], meta_backing);
    const data_nqn = try shareMulti(rig, "mpdata", data_len, &.{ "127.0.0.1", "127.0.0.2" }, ports[1], backing);
    const cm = try rig.connect(meta_nqn, .tcp, ports[0], null, &.{});
    const cm2 = try rig.connectAt(meta_nqn, .tcp, "127.0.0.2", ports[0], null, &.{});
    try testing.expectEqualStrings(cm.dev(), cm2.dev());
    try testing.expectEqual(@as(u32, 2), try rig.controllersOf(meta_nqn, ident_a));
    const cd = try rig.connect(data_nqn, .tcp, ports[1], null, &.{});
    var mu: [64]u8 = undefined;
    var du: [64]u8 = undefined;
    const meta_uri = try std.fmt.bufPrint(&mu, "sqmeta://{s}", .{cm.dev()});
    const data_uri = try std.fmt.bufPrint(&du, "sqdata://{s}", .{cd.dev()});
    const fmt_out = try rig.sqzOk(&.{ "format", "--single-writer", meta_uri, data_uri });
    gpa.free(fmt_out);
    // The record: both live paths and a dead one, one subsystem.
    {
        const show = try rig.sqzOk(&.{ "config", "show", "--json", meta_uri });
        defer gpa.free(show);
        const tag = json_scan.string(show, "vol_tag") orelse return error.NoVolTag;
        var spec: [1024]u8 = undefined;
        const word = try std.fmt.bufPrint(&spec, "{s}=tcp:127.0.0.1:{d}:{s},tcp:127.0.0.2:{d}:{s},tcp:127.0.0.3:{d}:{s}", .{ tag, ports[1], data_nqn, ports[1], data_nqn, ports[1], data_nqn });
        const out = try rig.sqzOk(&.{ "config", "set-fabric-endpoints", meta_uri, word });
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "(3 paths)") != null);
        const get = try rig.sqzOk(&.{ "config", "get-fabric-endpoints", "--json", meta_uri });
        defer gpa.free(get);
        try testing.expect(std.mem.indexOf(u8, get, "\"traddr\":\"127.0.0.1\"") != null and std.mem.indexOf(u8, get, "\"traddr\":\"127.0.0.2\"") != null and std.mem.indexOf(u8, get, "\"traddr\":\"127.0.0.3\"") != null);
        const get_text = try rig.sqzOk(&.{ "config", "get-fabric-endpoints", meta_uri });
        defer gpa.free(get_text);
        try testing.expectEqual(@as(usize, 3), std.mem.count(u8, get_text, "  path tcp:127.0.0."));
    }
    try rig.disconnect(&cd);
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(data_nqn, null));
    // Mount A under the DEFAULT policy (`round-robin`, owner ruling
    // 2026-10-18: every bio alternates, so both paths carry bytes whatever
    // the daemon's in-flight depth); two paths up, the dead one loud.
    const a = try rig.spawnMount("mp-a", meta_uri, null, &.{});
    try rig.awaitLog(a, "ready: ", ready_wait_ns);
    const st = try rig.stats(a);
    defer gpa.free(st);
    try testing.expectEqual(@as(?u64, 2), json_scan.number(st, "fabric_daemon_connects"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st, "fabric_daemon_reuses"));
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st, "fabric_paths_degraded"));
    // 2 data paths + the metadata head's 2 paths.
    try testing.expectEqual(@as(?u64, 4), json_scan.number(st, "fabric_paths_live"));
    try testing.expectEqual(@as(?u64, 2), json_scan.number(st, "fabric_iopolicy_sets"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st, "fabric_iopolicy_failures"));
    try testing.expect(std.mem.indexOf(u8, st, "\"iopolicy\":\"round-robin\",\"paths_failed\":1,\"paths\":[{\"instance\":") != null);
    {
        // The metadata head: judged per path under the pair, its policy
        // set, both paths listed live.
        var needle: [400]u8 = undefined;
        try testing.expect(std.mem.indexOf(u8, st, try std.fmt.bufPrint(&needle, "\"fabric_meta_bound\":[{{\"path\":\"{s}\",\"subsystem\":\"nvme-subsys", .{cm.dev()})) != null);
        try testing.expect(std.mem.indexOf(u8, st, try std.fmt.bufPrint(&needle, "\"subsysnqn\":\"{s}\",\"iopolicy\":\"round-robin\",\"paths\":[{{\"instance\":{d},\"transport\":\"tcp\",\"traddr\":\"127.0.0.1\",\"trsvcid\":\"{d}\",\"state\":\"live\"}},{{\"instance\":{d},\"transport\":\"tcp\",\"traddr\":\"127.0.0.2\",\"trsvcid\":\"{d}\",\"state\":\"live\"}}]}}", .{ meta_nqn, cm.instance, ports[0], cm2.instance, ports[0] })) != null);
        var msb: [32]u8 = undefined;
        const meta_sub = (try multipath.subsystemOf(gpa, initiator.SYSFS_ROOT, cm.instance, &msb)).?;
        var mpb: [32]u8 = undefined;
        try testing.expectEqualStrings("round-robin", multipath.readIopolicy(initiator.SYSFS_ROOT, meta_sub, &mpb).?);
        const log_a = try rig.readLog(a);
        defer gpa.free(log_a);
        try testing.expect(std.mem.indexOf(u8, log_a, "(2 path(s) under this mount's identity pair): iopolicy round-robin") != null);
        try testing.expect(std.mem.indexOf(u8, log_a, "fast_io_fail_tmo 45 s, ctrl_loss_tmo 90 s, reconnect_delay 10 s") != null);
    }
    try testing.expect(std.mem.indexOf(u8, st, "\"traddr\":\"127.0.0.1\"") != null and std.mem.indexOf(u8, st, "\"traddr\":\"127.0.0.2\"") != null);
    {
        const log_a = try rig.readLog(a);
        defer gpa.free(log_a);
        try testing.expect(std.mem.indexOf(u8, log_a, "DEGRADED — 1 of 3 path(s)") != null);
        try testing.expect(std.mem.indexOf(u8, log_a, "127.0.0.3") != null);
    }
    // Two controllers under our identity, ONE head: both hidden path nodes
    // name the same head and one subsystem dir lists both.
    var ib: [16]u32 = undefined;
    const insts = try instancesOf(rig, data_nqn, ident_a, &ib);
    try testing.expectEqual(@as(usize, 2), insts.len);
    var nodes: [16][32]u8 = @splat(@splat(0));
    var before: [16]multipath.BlockStat = undefined;
    try pathStats(rig, insts, &nodes, &before);
    const n0 = multipath.splitPathNodeName(std.mem.sliceTo(&nodes[0], 0)) orelse return error.NotAPathNode;
    const n1 = multipath.splitPathNodeName(std.mem.sliceTo(&nodes[1], 0)) orelse return error.NotAPathNode;
    try testing.expectEqual(n0.head, n1.head);
    var sb0: [32]u8 = undefined;
    var sb1: [32]u8 = undefined;
    const sub0 = (try multipath.subsystemOf(gpa, initiator.SYSFS_ROOT, insts[0], &sb0)).?;
    const sub1 = (try multipath.subsystemOf(gpa, initiator.SYSFS_ROOT, insts[1], &sb1)).?;
    try testing.expectEqualStrings(sub0, sub1);
    var hb: [32]u8 = undefined;
    var head_dev: [40]u8 = undefined;
    const head = (try multipath.headOf(gpa, initiator.SYSFS_ROOT, sub0, &hb)).?;
    try testing.expect(std.mem.indexOf(u8, st, try std.fmt.bufPrint(&head_dev, "\"device\":\"/dev/{s}\"", .{head})) != null);
    var pb: [32]u8 = undefined;
    try testing.expectEqualStrings("round-robin", multipath.readIopolicy(initiator.SYSFS_ROOT, sub0, &pb).?);
    const hv = try multipath.headVerdict(gpa, initiator.SYSFS_ROOT, insts, ident_a, &sb0);
    try testing.expectEqual(multipath.HeadVerdict.exactly_ours, hv.verdict);
    // `nvmeof status`: the subsystem, its policy, both paths.
    {
        const sj = try rig.sqzOk(&.{ "nvmeof", "status", "--json" });
        defer gpa.free(sj);
        var needle: [320]u8 = undefined;
        try testing.expect(std.mem.indexOf(u8, sj, try std.fmt.bufPrint(&needle, "\"subsysnqn\":\"{s}\",\"iopolicy\":\"round-robin\",\"head\":\"/dev/{s}\"", .{ data_nqn, head })) != null);
        try testing.expect(std.mem.indexOf(u8, sj, "\"traddr\":\"127.0.0.1\"") != null and std.mem.indexOf(u8, sj, "\"traddr\":\"127.0.0.2\"") != null);
        const stx = try rig.sqzOk(&.{ "nvmeof", "status" });
        defer gpa.free(stx);
        try testing.expect(std.mem.indexOf(u8, stx, try std.fmt.bufPrint(&needle, "{s} {s}\n  iopolicy round-robin; head {s}; 2 path(s)\n", .{ sub0, data_nqn, head })) != null);
    }
    // The write workload: four 64 MiB files in parallel, synced.
    const sums = try rig.onMount(a, "for i in 1 2 3 4; do head -c 67108864 /dev/urandom > p$i & done; wait; sync -d p1 p2 p3 p4 && cksum p1 p2 p3 p4 | cut -d' ' -f1");
    defer gpa.free(sums);
    var after_write: [16]multipath.BlockStat = undefined;
    try pathStats(rig, insts, &nodes, &after_write);
    const w0 = after_write[0].write_sectors - before[0].write_sectors;
    const w1 = after_write[1].write_sectors - before[1].write_sectors;
    std.debug.print("\nmultipath-tcp: {s} ({s}) round-robin (the default), 4 × 64 MiB written: write sectors per path nvme{d} {d}, nvme{d} {d}\n", .{ sub0, head, insts[0], w0, insts[1], w1 });
    try testing.expect(w0 > 0 and w1 > 0);
    // kill -9 → remount under an EXPLICIT `queue-depth` (the knob's other
    // arm, the measured line: a path is chosen by in-flight count and ties
    // go to the first, so a lone serial stream stays on one link): both
    // paths REUSED (never a second connect), the dead one degraded again,
    // both subsystems flipped to the knob; the cold serial read-back of
    // every file reaches the device (the sum is the assertion).
    try rig.kill9(a);
    try testing.expectEqual(@as(u32, 2), try rig.controllersOf(data_nqn, ident_a));
    var env_qd = try rig.envWithKnob("SQUEEZEFS_FABRIC_IOPOLICY", "queue-depth");
    defer env_qd.deinit();
    const a2 = try rig.spawnMountEnv("mp-a2", meta_uri, &env_qd, &.{});
    try rig.awaitLog(a2, "ready: ", ready_wait_ns);
    const st2 = try rig.stats(a2);
    defer gpa.free(st2);
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st2, "fabric_daemon_connects"));
    try testing.expectEqual(@as(?u64, 2), json_scan.number(st2, "fabric_daemon_reuses"));
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st2, "fabric_paths_degraded"));
    try testing.expectEqual(@as(?u64, 4), json_scan.number(st2, "fabric_paths_live"));
    try testing.expectEqual(@as(?u64, 2), json_scan.number(st2, "fabric_iopolicy_sets"));
    try testing.expect(std.mem.indexOf(u8, st2, "\"iopolicy\":\"queue-depth\",\"paths_failed\":1,") != null);
    try testing.expect(std.mem.indexOf(u8, st2, "\"iopolicy\":\"queue-depth\",\"paths\":[{\"instance\":") != null);
    try testing.expectEqualStrings("queue-depth", multipath.readIopolicy(initiator.SYSFS_ROOT, sub0, &pb).?);
    try testing.expectEqual(@as(u32, 2), try rig.controllersOf(data_nqn, ident_a));
    const sums2 = try rig.onMount(a2, "cksum p1 p2 p3 p4 | cut -d' ' -f1");
    defer gpa.free(sums2);
    try testing.expectEqualStrings(sums, sums2);
    var after_read: [16]multipath.BlockStat = undefined;
    try pathStats(rig, insts, &nodes, &after_read);
    const r0 = after_read[0].read_sectors - after_write[0].read_sectors;
    const r1 = after_read[1].read_sectors - after_write[1].read_sectors;
    std.debug.print("multipath-tcp: queue-depth (explicit), 4 × 64 MiB read cold by one serial reader: read sectors per path nvme{d} {d}, nvme{d} {d}\n", .{ insts[0], r0, insts[1], r1 });
    // 256 MiB of files = 524288 sectors of device reads at least.
    try testing.expect(r0 + r1 >= (4 * 64 << 20) / 512);
    // umount: every path released.
    try rig.umount(a2);
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(data_nqn, null));
    {
        const log2 = try rig.readLog(a2);
        defer gpa.free(log2);
        try testing.expect(std.mem.indexOf(u8, log2, "2 controller(s) of ") != null and std.mem.indexOf(u8, log2, "under this mount's identity released") != null);
    }
    // Mount A3 under the DEFAULT again: fresh connects, both subsystems
    // back to `round-robin`, four parallel cold readers land read sectors
    // on BOTH paths.
    const a3 = try rig.spawnMount("mp-a3", meta_uri, null, &.{});
    try rig.awaitLog(a3, "ready: ", ready_wait_ns);
    const st3 = try rig.stats(a3);
    defer gpa.free(st3);
    try testing.expectEqual(@as(?u64, 2), json_scan.number(st3, "fabric_daemon_connects"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st3, "fabric_daemon_reuses"));
    try testing.expectEqual(@as(?u64, 4), json_scan.number(st3, "fabric_paths_live"));
    try testing.expect(std.mem.indexOf(u8, st3, "\"iopolicy\":\"round-robin\",\"paths_failed\":1,") != null);
    try testing.expect(std.mem.indexOf(u8, st3, "\"iopolicy\":\"round-robin\",\"paths\":[{\"instance\":") != null);
    const insts3 = try instancesOf(rig, data_nqn, ident_a, &ib);
    try testing.expectEqual(@as(usize, 2), insts3.len);
    var sb3: [32]u8 = undefined;
    const sub3 = (try multipath.subsystemOf(gpa, initiator.SYSFS_ROOT, insts3[0], &sb3)).?;
    try testing.expectEqualStrings("round-robin", multipath.readIopolicy(initiator.SYSFS_ROOT, sub3, &pb).?);
    var before3: [16]multipath.BlockStat = undefined;
    try pathStats(rig, insts3, &nodes, &before3);
    const sums3 = try rig.onMount(a3, "for f in p1 p2 p3 p4; do cat $f > /dev/null & done; wait; cksum p1 p2 p3 p4 | cut -d' ' -f1");
    defer gpa.free(sums3);
    try testing.expectEqualStrings(sums, sums3);
    var after3: [16]multipath.BlockStat = undefined;
    try pathStats(rig, insts3, &nodes, &after3);
    const q0 = after3[0].read_sectors - before3[0].read_sectors;
    const q1 = after3[1].read_sectors - before3[1].read_sectors;
    std.debug.print("multipath-tcp: round-robin (the default), 4 × 64 MiB read cold by 4 parallel readers: read sectors per path nvme{d} {d}, nvme{d} {d}\n", .{ insts3[0], q0, insts3[1], q1 });
    try testing.expect(q0 > 0 and q1 > 0);
    try testing.expect(q0 + q1 >= (4 * 64 << 20) / 512);
    try rig.umount(a3);
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(data_nqn, null));
    // The metadata paths are the operator's: both still stand after the
    // umount, one `--identity-only` disconnect releases both.
    try testing.expectEqual(@as(u32, 2), try rig.controllersOf(meta_nqn, ident_a));
    try rig.disconnect(&cm);
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(meta_nqn, null));
    try rig.unshare(data_nqn);
    try rig.unshare(meta_nqn);
    try expectNoResidue(rig, meta_nqn);
    try expectNoResidue(rig, data_nqn);
}

test "nvmet fidelity: fidelity-mount (loop) — THE row: the metadata and data volumes ARE connected namespaces (the data volume the daemon's own connect from its endpoint record); format → mount → PR armed for real (holder == pr_key, regctl 1) → a second identity refused → kill -9 → remount adopts the host's standing registration under the same host key (pr_host_adoptions 1, pr_own_stale_recoveries 0, payload intact) and REUSES the data association → umount → the inherited hold stays (regctl 1, rtype 1: review 4 blocker 2), the association released → disconnect → unshare" {
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
    // Synced by the file, so A's write has landed before the STOP (§DB.3).
    const w0 = try rig.onMount(a, "echo before > f && sync -d f");
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
    const wb = try rig.onMount(b, "echo after > g && sync -f . && cat f");
    defer gpa.free(wb);
    try testing.expectEqualStrings("before\n", wb);
    // Resume A: its next beat finds a foreign holder — fenced; the latch
    // reads failed and B's writes keep landing.
    _ = linux.kill(rig.daemons.items[a].pid, .CONT);
    const seen = try rig.awaitStat(a, "writer_guard_fenced", 1, 30 * std.time.ns_per_s);
    gpa.free(seen);
    // A render writes the volume's verdict (`meta.set`) BEFORE the
    // process-wide counter (`meta.guard`), and the fence latches the verdict
    // before it counts: the render that first shows the count may have read
    // the verdict ahead of the latch. Both are judged on a render begun
    // after the count was seen — the latch never clears, the count is paid
    // once per latch.
    const sta = try rig.stats(a);
    defer gpa.free(sta);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(sta, "writer_guard_fenced"));
    // The latch's verdict: FAILED by the fence class (`fenced`).
    try testing.expectEqualStrings("fenced", json_scan.string(sta, "writer_guard_verdict").?);
    const wb2 = try rig.onMount(b, "echo again > h && sync -f . && cat h");
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
    const sum = try rig.onMount(a, "head -c 4194304 /dev/urandom > payload && sync -f . && cksum payload | cut -d' ' -f1");
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

/// `sh -c script` on a daemon's mount: whether it exited 0 (a refused
/// write is the expected answer of a fenced mount).
fn scriptSucceeds(rig: *Rig, d: usize, script: []const u8) !bool {
    const r = try std.process.run(rig.gpa, rig.io, .{ .argv = &.{ "sh", "-c", script }, .cwd = .{ .path = rig.daemons.items[d].mp }, .environ_map = &rig.env });
    defer rig.freeRun(r);
    return r.term.success();
}

/// A stats number that must be present (a missing key is the red's answer,
/// never a panic in the runner).
fn statNumber(text: []const u8, key: []const u8) !u64 {
    return json_scan.number(text, key) orelse {
        std.debug.print("\n--- .stats carries no \"{s}\" ---\n", .{key});
        return error.StatMissing;
    };
}

test "nvmet fidelity: co-located mounts — ONE host identity for every mount of a set on a host (owner ruling 2026-11-02, option C): a symmetric set on nvmet-loop, the manager A and a joiner B on this host under the one identity; ONE controller of the data subsystem serves both; ONE registration of the host under the manager's WERO hold, beside the manager's fencing registrant under the same key, never holding (regctl 2, decision 0003 §DG), A registered it, B adopted it (the same key, no device mutation); both read each other's files; B's umount leaves the shared association and the registration standing (A reads B's file and writes on), A's umount releases them all (no controller, regctl 0)" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "co", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    const a = try rig.spawnMount("co-a", set.metaUri(), null, &.{});
    try rig.awaitLog(a, "ready: ", ready_wait_ns);
    const b = try rig.spawnMount("co-b", set.metaUri(), null, &.{});
    try rig.awaitLog(b, "ready: ", ready_wait_ns);
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(set.data_nqn, ident_a));
    {
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, 2), json_scan.number(rr, "regctl"));
        try testing.expectEqual(@as(?u64, nvme.RTYPE_WERO), json_scan.number(rr, "rtype"));
        const sa = try rig.stats(a);
        defer gpa.free(sa);
        const sb = try rig.stats(b);
        defer gpa.free(sb);
        try testing.expectEqual(@as(u64, 1), try statNumber(sa, "pr_host_registrations"));
        try testing.expectEqual(@as(u64, 0), try statNumber(sb, "pr_host_registrations"));
        try testing.expectEqual(@as(u64, 1), try statNumber(sb, "pr_host_adoptions"));
        // The manager stood its fencer; the joiner none.
        try testing.expectEqual(@as(u64, 1), try statNumber(sa, "pr_fencer_registrations"));
        try testing.expectEqual(@as(u64, 0), try statNumber(sb, "pr_fencer_registrations"));
        const key_a = json_scan.string(sa, "pr_key") orelse return error.StatMissing;
        const key_b = json_scan.string(sb, "pr_key") orelse return error.StatMissing;
        try testing.expectEqualStrings(key_a, key_b);
        try testing.expectEqualStrings(key_a, json_scan.string(rr, "holder_key") orelse return error.StatMissing);
        // holder_key + the host and its fencer under the one key, one holding.
        try testing.expectEqual(@as(usize, 3), std.mem.count(u8, rr, key_a));
        try testing.expectEqual(@as(usize, 1), std.mem.count(u8, rr, "\"holds_reservation\":true"));
    }
    const sum_a = try rig.onMount(a, "head -c 1048576 /dev/urandom > fa && sync -d fa && cksum fa | cut -d' ' -f1");
    defer gpa.free(sum_a);
    const seen_b = try rig.onMount(b, "cksum fa | cut -d' ' -f1");
    defer gpa.free(seen_b);
    try testing.expectEqualStrings(sum_a, seen_b);
    const sum_b = try rig.onMount(b, "head -c 1048576 /dev/urandom > fb && sync -d fb && cksum fb | cut -d' ' -f1");
    defer gpa.free(sum_b);
    // B leaves: the association and the registration are the host's, and
    // A still mounts on them.
    try rig.umount(b);
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(set.data_nqn, ident_a));
    try testing.expect(try scriptSucceeds(rig, a, "cksum fb > /dev/null && head -c 1048576 /dev/urandom > fc && sync -d fc"));
    const seen_a = try rig.onMount(a, "cksum fb | cut -d' ' -f1");
    defer gpa.free(seen_a);
    try testing.expectEqualStrings(sum_b, seen_a);
    {
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, 2), json_scan.number(rr, "regctl"));
    }
    try rig.umount(a);
    try testing.expectEqual(@as(u32, 0), try rig.controllersOf(set.data_nqn, null));
    // The fencer's controller went with its last user: the rig's own alone.
    try testing.expectEqual(@as(u32, 1), try rig.controllersOf(set.meta_nqn, null));
    {
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, 0), json_scan.number(rr, "regctl"));
    }
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

/// Open a FIFO for write: a daemon parked opening it for read proceeds.
fn releaseFifo(path: [:0]const u8) void {
    const rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) == .SUCCESS) _ = linux.close(@intCast(rc));
}

/// Wait until daemon `d` either logged `ready: ` or exited; true = ready.
fn readyOrDead(rig: *Rig, d: usize) !bool {
    const deadline = clock.monotonicNs() + ready_wait_ns;
    while (clock.monotonicNs() < deadline) {
        const text = try rig.readLog(d);
        defer rig.gpa.free(text);
        if (std.mem.indexOf(u8, text, "ready: ") != null) return true;
        if (!alive(rig.daemons.items[d].pid)) return false;
        sleepMs(50);
    }
    return error.DaemonNeitherReadyNorDead;
}

const ident_c: initiator.HostPair = .{ .hostnqn = "nqn.2014-08.org.nvmexpress:uuid:c0c0c0c0-c0c0-4c0c-8c0c-c0c0c0c0c0c0", .hostid = "c0c0c0c0-c0c0-4c0c-8c0c-c0c0c0c0c0c0" };

test "nvmet fidelity: stale sibling — one manager per volume and per-host fencing's safety rule on a three-host set: B (host A's manager) and A (a joiner on host A) under host A's one identity (B registers the host, A adopts); B frozen (SIGSTOP) past the claim's TTL; two successors on hosts B and C race for the role — P1 arbitrates and parks before its claim (SQUEEZEFS_TEST_GATE_ARBITRATED_HOLD), P2 arbitrates meanwhile: the device keeps ONE holder (WERO), so P2's acquire conflicts and preempts P1's host, P2 binds and P1, released, never does (review round 3 N1: under rtype 5 both bound); host A was fenced by the takeover — A's fsync refused; a fresh mount on host A whose kill of B is refused (SQUEEZEFS_TEST_SIBLING_KILL_REFUSED) refuses its registration loud by name and exits, B alive; A2 kills the superseded manager B (and A, if evicted meanwhile) before it registers host A, joins P2 and writes; P2 reads it" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "ss", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    const cm_c = try rig.connect(set.meta_nqn, transport, 0, ident_c, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var mu_c: [64]u8 = undefined;
    const meta_uri_c = try std.fmt.bufPrint(&mu_c, "sqmeta://{s}", .{cm_c.dev()});
    // Host A: the manager B, then the joiner A.
    const b = try rig.spawnMount("ss-b", set.metaUri(), null, &.{});
    try rig.awaitLog(b, "ready: ", ready_wait_ns);
    const a = try rig.spawnMount("ss-a", set.metaUri(), null, &.{});
    try rig.awaitLog(a, "ready: ", ready_wait_ns);
    {
        const sb = try rig.stats(b);
        defer gpa.free(sb);
        try testing.expectEqual(@as(u64, 1), try statNumber(sb, "pr_host_registrations"));
        const sa = try rig.stats(a);
        defer gpa.free(sa);
        try testing.expectEqual(@as(u64, 1), try statNumber(sa, "pr_host_adoptions"));
        // Host A and the manager B's fencer under host A's key.
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, 2), json_scan.number(rr, "regctl"));
    }
    try testing.expect(try scriptSucceeds(rig, a, "mkdir da && echo a > da/a && sync -d da/a"));
    const before = try rig.onMount(b, "echo before > f && sync -d f && cat f");
    defer gpa.free(before);
    try testing.expectEqualStrings("before\n", before);
    // B frozen: its claim ages past the TTL (its beats stop).
    const pid_b = rig.daemons.items[b].pid;
    _ = linux.kill(pid_b, .STOP);
    sleepMs(lease_clock.T_OWNER_MS + 5_000);
    // Two successors race for the manager role.
    var fb: [128]u8 = undefined;
    const fifo = try std.fmt.bufPrintSentinel(&fb, "{s}/p1-arbitrated.fifo", .{rig.root}, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.mknodat(linux.AT.FDCWD, fifo.ptr, linux.S.IFIFO | 0o600, 0)));
    var env_p1 = try rig.envWith(ident_b);
    defer env_p1.deinit();
    try env_p1.put("SQUEEZEFS_TEST_GATE_ARBITRATED_HOLD", fifo);
    const p1 = try rig.spawnMountEnv("ss-p1", meta_uri_b, &env_p1, &.{});
    try rig.awaitLog(p1, "SQUEEZEFS_TEST_GATE_ARBITRATED_HOLD — the gate holds", ready_wait_ns);
    const p2 = try rig.spawnMount("ss-p2", meta_uri_c, ident_c, &.{});
    const p2_ready = try readyOrDead(rig, p2);
    releaseFifo(fifo);
    const p1_ready = try readyOrDead(rig, p1);
    // ONE manager: the later arbiter (P2) preempted P1's host; P1 never binds.
    try testing.expect(p2_ready);
    try testing.expect(!p1_ready);
    // Host A was fenced by the takeover (B's key, host A's, preempted): A's
    // fsync is refused at the device (or A, parked past its bound, is gone).
    const a_wrote = scriptSucceeds(rig, a, "echo x > da/z && sync -d da/z") catch false;
    try testing.expect(!a_wrote);
    {
        // Host C and P2's fencer: every takeover preempt took the superseded
        // host's fencer with it (host A's, then P1's host B's).
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, 2), json_scan.number(rr, "regctl"));
    }
    // A stale sibling that can be neither proven dead nor killed refuses the
    // registration loud by name: the mount exits, B lives on, host A stays
    // unregistered.
    {
        var env_r = try rig.envWithKnob("SQUEEZEFS_TEST_SIBLING_KILL_REFUSED", "1");
        defer env_r.deinit();
        const r = try rig.spawnMountEnv("ss-refused", set.metaUri(), &env_r, &.{});
        try testing.expect(!try readyOrDead(rig, r));
        const dm = &rig.daemons.items[r];
        const term = try dm.child.wait(rig.io);
        dm.reaped = true;
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, term);
        const text = try rig.readLog(r);
        defer gpa.free(text);
        // B, or A (evicted while its manager was gone, its region recovered by
        // P2) — whichever stale sibling the rule meets first is named.
        const named = std.mem.indexOf(u8, text, "REFUSING to register this host: the stale sibling pid ") != null;
        if (!named) std.debug.print("\n--- the refused mount's log ---\n{s}\n", .{text});
        try testing.expect(named);
        try testing.expect(alive(pid_b));
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, 2), json_scan.number(rr, "regctl"));
    }
    // A, fenced with its host, is stopped by hand: whether P2 evicted it
    // yet is timing, so the rule below meets exactly one live stale sibling.
    try rig.kill9(a);
    // A2 re-registers host A — never while the superseded B lives.
    const a2 = try rig.spawnMount("ss-a2", set.metaUri(), null, &.{});
    try rig.awaitLog(a2, "ready: ", ready_wait_ns);
    try testing.expect(!alive(pid_b));
    {
        const s2 = try rig.stats(a2);
        defer gpa.free(s2);
        try testing.expectEqual(@as(u64, 1), try statNumber(s2, "pr_stale_sibling_kills"));
        try testing.expectEqual(@as(u64, 1), try statNumber(s2, "pr_host_registrations"));
        try testing.expectEqual(@as(u64, 0), try statNumber(s2, "pr_stale_sibling_refusals"));
    }
    const after = try rig.onMount(a2, "mkdir d2 && echo after > d2/g && sync -d d2/g && cat f d2/g");
    defer gpa.free(after);
    try testing.expectEqualStrings("before\nafter\n", after);
    const seen = try rig.onMount(p2, "cat d2/g");
    defer gpa.free(seen);
    try testing.expectEqualStrings("after\n", seen);
    try rig.kill9(b);
    try rig.kill9(p1);
    try rig.umount(a2);
    try rig.umount(p2);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_c);
    try rig.disconnect(&cm_c);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

test "nvmet fidelity: a remote joiner never registers or writes before its claim check (review round 3 major 2) — M on host B is the manager; J on host A mounts over M's fresh claim: J's writer attempt refuses before its host registers or its open writes (a held tail, the replay's fold), so the report's generation moves by J's one registration as a joiner" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "rj", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    const m = try rig.spawnMount("rj-m", meta_uri_b, ident_b, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    try testing.expect(try scriptSucceeds(rig, m, "mkdir dm && echo m > dm/m && sync -d dm/m"));
    const gen0 = blk: {
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        break :blk json_scan.number(rr, "generation") orelse return error.StatMissing;
    };
    const j = try rig.spawnMount("rj-j", set.metaUri(), null, &.{});
    try rig.awaitLog(j, "ready: ", ready_wait_ns);
    const gen1 = blk: {
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        break :blk json_scan.number(rr, "generation") orelse return error.StatMissing;
    };
    try testing.expectEqual(gen0 + 1, gen1);
    const seen = try rig.onMount(j, "cat dm/m");
    defer gpa.free(seen);
    try testing.expectEqualStrings("m\n", seen);
    try rig.umount(j);
    try rig.umount(m);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

test "nvmet fidelity: a superseded manager never wins the hold back (review round 3 major 3) nor writes over its successor's acked data (review 4 blocker 2) — M0 (host A), its last files unsynced, frozen past its claim's TTL; the successor S (host B) takes over by a preempt, writes ds/s and unmounts cleanly: a hold taken by a preempt STAYS (regctl 1, rtype 3 — the next mount preempts or adopts it; releasing it re-admitted every host the preempt fenced); S2 on host B adopts the standing hold, reads ds/s, writes ds/t and unmounts: the inherited hold stays too; M0 resumed is refused at its first write or its beat (writer_guard_fenced 1, writer_guard_pr_reacquires 0) — its checkpoint lands nothing; a remount reads ds/s and ds/t back" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "lp", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    const m0 = try rig.spawnMount("lp-m0", set.metaUri(), null, &.{});
    try rig.awaitLog(m0, "ready: ", ready_wait_ns);
    try testing.expect(try scriptSucceeds(rig, m0, "echo before > f && sync -d f"));
    // Unsynced: M0's checkpoint has nodes, bitmap pages and a ledger record
    // to land when it resumes.
    try testing.expect(try scriptSucceeds(rig, m0, "mkdir d0 && for i in $(seq 1 200); do echo $i > d0/f$i; done"));
    const pid_m0 = rig.daemons.items[m0].pid;
    _ = linux.kill(pid_m0, .STOP);
    sleepMs(lease_clock.T_OWNER_MS + 5_000);
    const s = try rig.spawnMount("lp-s", meta_uri_b, ident_b, &.{});
    try rig.awaitLog(s, "ready: ", ready_wait_ns);
    try testing.expect(try scriptSucceeds(rig, s, "mkdir ds && echo s > ds/s && sync -d ds/s"));
    try rig.umount(s);
    try expectHoldStands(rig, cm.dev());
    const s2 = try rig.spawnMount("lp-s2", meta_uri_b, ident_b, &.{});
    try rig.awaitLog(s2, "ready: ", ready_wait_ns);
    {
        const seen = try rig.onMount(s2, "cat ds/s");
        defer gpa.free(seen);
        try testing.expectEqualStrings("s\n", seen);
    }
    try testing.expect(try scriptSucceeds(rig, s2, "echo t > ds/t && sync -d ds/t"));
    try rig.umount(s2);
    try expectHoldStands(rig, cm.dev());
    _ = linux.kill(pid_m0, .CONT);
    const st = try rig.awaitStat(m0, "writer_guard_fenced", 1, 30 * std.time.ns_per_s);
    defer gpa.free(st);
    try testing.expectEqual(@as(?u64, 1), json_scan.number(st, "writer_guard_fenced"));
    try testing.expectEqual(@as(?u64, 0), json_scan.number(st, "writer_guard_pr_reacquires"));
    const m0_wrote = scriptSucceeds(rig, m0, "echo x > z && sync -d z") catch false;
    try testing.expect(!m0_wrote);
    try rig.kill9(m0);
    const r = try rig.spawnMount("lp-r", meta_uri_b, ident_b, &.{});
    try rig.awaitLog(r, "ready: ", ready_wait_ns);
    {
        const seen = try rig.onMount(r, "cat ds/s ds/t");
        defer gpa.free(seen);
        try testing.expectEqualStrings("s\nt\n", seen);
    }
    try rig.umount(r);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

/// After a takeover successor's clean last unmount: its hold stands under
/// WERO with its host's one registration (review 4 blocker 2).
fn expectHoldStands(rig: *Rig, dev: []const u8) !void {
    const rr = try rig.resvReportJson(dev);
    defer rig.gpa.free(rr);
    const regctl = json_scan.number(rr, "regctl");
    const rtype = json_scan.number(rr, "rtype");
    if (regctl != 1 or rtype != nvme.RTYPE_WERO) std.debug.print("\n--- the report after the takeover's last unmount: {s} ---\n", .{rr});
    try testing.expectEqual(@as(?u64, 1), regctl);
    try testing.expectEqual(@as(?u64, nvme.RTYPE_WERO), rtype);
}

test "nvmet fidelity: a live manager that took over during another open's rule is never preempted (review 5 major 1) — X (host A) parks after its first claim probe (SQUEEZEFS_TEST_EARLY_PROBE_HOLD, no claim yet: a rule that takes its 30 s lock, 10 s kill and 10 s drain); W (host B) mounts, binds and writes dw/w; X, released, reads the claim again right before its hold, finds W's fresh claim and refuses its writer open WITHOUT a preempt — it joins W: W's host stays registered and holding, W writes on (writer_guard_fenced 0), X reads W's files and preempted nothing" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "pm", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var fb: [128]u8 = undefined;
    const fifo = try std.fmt.bufPrintSentinel(&fb, "{s}/x-probed.fifo", .{rig.root}, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.mknodat(linux.AT.FDCWD, fifo.ptr, linux.S.IFIFO | 0o600, 0)));
    var env_x = try rig.envWithKnob("SQUEEZEFS_TEST_EARLY_PROBE_HOLD", fifo);
    defer env_x.deinit();
    const x = try rig.spawnMountEnv("pm-x", set.metaUri(), &env_x, &.{});
    try rig.awaitLog(x, "SQUEEZEFS_TEST_EARLY_PROBE_HOLD — the open holds", ready_wait_ns);
    const w = try rig.spawnMount("pm-w", meta_uri_b, ident_b, &.{});
    try rig.awaitLog(w, "ready: ", ready_wait_ns);
    try testing.expect(try scriptSucceeds(rig, w, "mkdir dw && echo w > dw/w && sync -d dw/w"));
    const w_key = blk: {
        const sw = try rig.stats(w);
        defer gpa.free(sw);
        break :blk try gpa.dupe(u8, json_scan.string(sw, "pr_key") orelse return error.StatMissing);
    };
    defer gpa.free(w_key);
    releaseFifo(fifo);
    try testing.expect(try readyOrDead(rig, x));
    {
        const sx = try rig.stats(x);
        defer gpa.free(sx);
        try testing.expectEqual(@as(u64, 0), try statNumber(sx, "writer_guard_preempts"));
    }
    try testing.expect(try scriptSucceeds(rig, w, "echo y > dw/y && sync -d dw/y"));
    {
        const sw = try rig.stats(w);
        defer gpa.free(sw);
        try testing.expectEqual(@as(u64, 0), try statNumber(sw, "writer_guard_fenced"));
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqualStrings(w_key, json_scan.string(rr, "holder_key") orelse return error.StatMissing);
    }
    const seen = try rig.onMount(x, "cat dw/w dw/y");
    defer gpa.free(seen);
    try testing.expectEqualStrings("w\ny\n", seen);
    try rig.umount(x);
    try rig.umount(w);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

test "nvmet fidelity: an open whose hold's preempt fenced the writer that committed its claim in the instant before the hold takes over (review 4 minor 4) — O (host A) parks after its second claim probe, right before its hold (SQUEEZEFS_TEST_EARLY_HOLD_PARK, no claim yet); W (host B) mounts, binds and writes dw/w; O, released, preempts W's host at its hold, meets W's fresh claim at its third probe and proceeds: O binds as the manager and reads dw/w, W's writes are refused" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "pp", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var fb: [128]u8 = undefined;
    const fifo = try std.fmt.bufPrintSentinel(&fb, "{s}/o-prehold.fifo", .{rig.root}, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.mknodat(linux.AT.FDCWD, fifo.ptr, linux.S.IFIFO | 0o600, 0)));
    var env_o = try rig.envWithKnob("SQUEEZEFS_TEST_EARLY_HOLD_PARK", fifo);
    defer env_o.deinit();
    const o = try rig.spawnMountEnv("pp-o", set.metaUri(), &env_o, &.{});
    try rig.awaitLog(o, "SQUEEZEFS_TEST_EARLY_HOLD_PARK — the open holds", ready_wait_ns);
    const w = try rig.spawnMount("pp-w", meta_uri_b, ident_b, &.{});
    try rig.awaitLog(w, "ready: ", ready_wait_ns);
    try testing.expect(try scriptSucceeds(rig, w, "mkdir dw && echo w > dw/w && sync -d dw/w"));
    releaseFifo(fifo);
    try testing.expect(try readyOrDead(rig, o));
    {
        const so = try rig.stats(o);
        defer gpa.free(so);
        try testing.expectEqual(@as(?bool, true), json_scan.boolean(so, "writer_guard_claimed"));
        try testing.expectEqual(@as(u64, 1), try statNumber(so, "writer_guard_preempts"));
    }
    const seen = try rig.onMount(o, "cat dw/w");
    defer gpa.free(seen);
    try testing.expectEqualStrings("w\n", seen);
    const w_wrote = scriptSucceeds(rig, w, "echo x > dw/z && sync -d dw/z") catch false;
    try testing.expect(!w_wrote);
    try rig.kill9(w);
    try rig.umount(o);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

/// The ring of the probe-cost row: a 256 MiB override on the 1 GiB meta
/// volume (the derived ring caps at 32 MiB; `--meta-journal-mb` reaches 1 GiB).
const probe_cost_ring_mib: u64 = 256;

test "nvmet fidelity: the early claim probes' cost on a large ring, as counter deltas (review 4 minor 6) — a symmetric set formatted with a 256 MiB ring: every NVMe writer open replays the claim privately three times (the probe before the rule, right before the hold, under the hold — review 5 major 1), each reading both superblock copies, the ledger and the WHOLE ring; the first open and a kill -9 remount each count 3 probes, ≥ 3 × the ring in bytes, and the frames they replayed (printed)" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    var jb: [16]u8 = undefined;
    const ring_mib = try std.fmt.bufPrint(&jb, "{d}", .{probe_cost_ring_mib});
    const set = try daemonSetFormat(rig, "pc", transport, try rig.servicePorts(transport), &.{}, &.{ "--meta-journal-mb", ring_mib });
    const cm = set.cm;
    const ring_bytes = probe_cost_ring_mib << 20;
    const a = try rig.spawnMount("pc-a", set.metaUri(), null, &.{});
    try rig.awaitLog(a, "ready: ", ready_wait_ns);
    const first = try probeCost(rig, a);
    try testing.expect(try scriptSucceeds(rig, a, "mkdir d && for i in $(seq 1 2000); do echo $i > d/f$i; done && sync -d d/f2000"));
    try rig.kill9(a);
    const a2 = try rig.spawnMount("pc-a2", set.metaUri(), null, &.{});
    try rig.awaitLog(a2, "ready: ", ready_wait_ns);
    const second = try probeCost(rig, a2);
    std.debug.print("\nprobe cost (ring {d} MiB): first open {d} probes, {d} B, {d} frames; kill -9 remount {d} probes, {d} B, {d} frames\n", .{ probe_cost_ring_mib, first.probes, first.bytes, first.frames, second.probes, second.bytes, second.frames });
    for ([_]ProbeCost{ first, second }) |c| {
        try testing.expectEqual(@as(u64, 3), c.probes);
        try testing.expect(c.bytes >= 3 * ring_bytes - 3 * (16 << 10));
    }
    try rig.umount(a2);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

const ProbeCost = struct { probes: u64, bytes: u64, frames: u64 };

fn probeCost(rig: *Rig, d: usize) !ProbeCost {
    const st = try rig.stats(d);
    defer rig.gpa.free(st);
    return .{ .probes = try statNumber(st, "writer_guard_claim_probes"), .bytes = try statNumber(st, "writer_guard_claim_probe_bytes"), .frames = try statNumber(st, "writer_guard_claim_probe_frames") };
}

/// The membership `T_owner` of the member-death row (the smoke rows' value).
const member_t_owner_ms: u64 = 3000;

test "nvmet fidelity: a member's death on nvmet 7.3 is FENCED by the manager's fencing registrant (decision 0003 §DG) — a symmetric set whose manager M (host B) holds WERO and stood its fencer (a second identity of host B under M's key, never holding); the member X on host A, frozen past T_owner, is evicted; M's fencer — a NON-holder — preempts host A's key (nvmet's spec path): host A unregistered, the reservation kept under M's key (appender_recovery_preempts 1, pr_fencer_preempts 1, appender_recovery_preempts_ignored 0, no re-fence; the report: M's host holding beside its fencer under the same key, host A's key gone)" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "md", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    var tb: [16]u8 = undefined;
    const t_owner = try std.fmt.bufPrint(&tb, "{d}", .{member_t_owner_ms});
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var env_m = try rig.envWith(ident_b);
    defer env_m.deinit();
    try env_m.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    var env_x = try rig.envWithKnob("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    defer env_x.deinit();
    const m = try rig.spawnMountEnv("md-m", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    const x = try rig.spawnMountEnv("md-x", set.metaUri(), &env_x, &.{});
    try rig.awaitLog(x, "ready: ", ready_wait_ns);
    // X's own directory: its creates land through X's own ring and slot.
    try testing.expect(try scriptSucceeds(rig, x, "mkdir dx && echo x > dx/x && sync -d dx/x"));
    const key_x = try statString(rig, x, "pr_key");
    defer gpa.free(key_x);
    const key_m = try statString(rig, m, "pr_key");
    defer gpa.free(key_m);
    {
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, nvme.RTYPE_WERO), json_scan.number(rr, "rtype"));
        try testing.expect(std.mem.indexOf(u8, rr, key_x) != null);
    }
    _ = linux.kill(rig.daemons.items[x].pid, .STOP);
    const sm = try rig.awaitStat(m, "appender_recoveries", 1, ready_wait_ns);
    defer gpa.free(sm);
    try testing.expectEqual(@as(u64, 1), try statNumber(sm, "appender_recoveries"));
    // The death preempt LANDED, through the fencer: no ignored preempt, and
    // the key stood absent once the page was Recovering (no re-fence).
    try testing.expectEqual(@as(u64, 1), try statNumber(sm, "appender_recovery_preempts"));
    try testing.expectEqual(@as(u64, 0), try statNumber(sm, "appender_recovery_preempts_ignored"));
    try testing.expectEqual(@as(u64, 0), try statNumber(sm, "appender_recovery_refences"));
    try testing.expectEqual(@as(u64, 1), try statNumber(sm, "pr_fencer_preempts"));
    try testing.expectEqual(@as(u64, 1), try statNumber(sm, "pr_fencer_registrations"));
    {
        // Host A unregistered (the frozen X, resumed, is refused at the
        // device); the reservation kept: M's host holding, its fencer beside
        // it under the same key, not holding.
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expect(std.mem.indexOf(u8, rr, key_x) == null);
        try testing.expectEqual(@as(?u64, nvme.RTYPE_WERO), json_scan.number(rr, "rtype"));
        try testing.expectEqualStrings(key_m, json_scan.string(rr, "holder_key") orelse return error.StatMissing);
        try testing.expectEqual(@as(?u64, 2), json_scan.number(rr, "regctl"));
        // holder_key + the two registrants, one holding.
        try testing.expectEqual(@as(usize, 3), std.mem.count(u8, rr, key_m));
        try testing.expectEqual(@as(usize, 1), std.mem.count(u8, rr, "\"holds_reservation\":true"));
    }
    try rig.kill9(x);
    try rig.umount(m);
    // M's umount released its fencer (the last user): its registration and
    // its controller gone, the hold kept (a preempt landed under it).
    {
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(?u64, 1), json_scan.number(rr, "regctl"));
        try testing.expectEqualStrings(key_m, json_scan.string(rr, "holder_key") orelse return error.StatMissing);
    }
    try testing.expectEqual(@as(u32, 2), try rig.controllersOf(set.meta_nqn, null));
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

test "nvmet fidelity: a frozen member fenced through the manager's fencing registrant never lands its in-flight writes (decision 0003 §DG) — X (host A) writes behind a held landing (SQUEEZEFS_TEST_LANDING_HOLD_AT_WRITE: its acked windows admitted, never issued) and is SIGSTOPped; M (host B) evicts it at T_owner and fences host A through its fencer; X's held windows released (SQUEEZEFS_TEST_LANDING_HOLD_RELEASE) and X resumed: the device refuses them (a reservation conflict, -EBADE): X latches the fence class (its journal's reservation-conflict line) and never lands them" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "mr", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    var tb: [16]u8 = undefined;
    const t_owner = try std.fmt.bufPrint(&tb, "{d}", .{member_t_owner_ms});
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var fb: [128]u8 = undefined;
    const fifo = try std.fmt.bufPrintSentinel(&fb, "{s}/x-landing.fifo", .{rig.root}, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.mknodat(linux.AT.FDCWD, fifo.ptr, linux.S.IFIFO | 0o600, 0)));
    var env_m = try rig.envWith(ident_b);
    defer env_m.deinit();
    try env_m.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    var env_x = try rig.envWithKnob("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    defer env_x.deinit();
    // X's second FUSE WRITE arms the hold: that write's commit and every
    // later one admitted and acked, never issued until the FIFO opens.
    try env_x.put("SQUEEZEFS_TEST_LANDING_HOLD_AT_WRITE", "2");
    try env_x.put("SQUEEZEFS_TEST_LANDING_HOLD_RELEASE", fifo);
    const m = try rig.spawnMountEnv("mr-m", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    const x = try rig.spawnMountEnv("mr-x", set.metaUri(), &env_x, &.{});
    try rig.awaitLog(x, "ready: ", ready_wait_ns);
    try testing.expect(try scriptSucceeds(rig, x, "mkdir dx && echo a > dx/a && sync -d dx/a && echo b > dx/b"));
    try rig.awaitLog(x, "test seam: SQUEEZEFS_TEST_LANDING_HOLD_AT_WRITE", ready_wait_ns);
    // The release's read end is open before the STOP: the release below
    // never waits on X being scheduled.
    try rig.awaitLog(x, "SQUEEZEFS_TEST_LANDING_HOLD_RELEASE — the release waits", ready_wait_ns);
    _ = linux.kill(rig.daemons.items[x].pid, .STOP);
    {
        const sm = try rig.awaitStat(m, "appender_recoveries", 1, ready_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "appender_recoveries"));
    }
    // The held windows released while X is stopped (the FIFO's writer opens
    // and closes beside X's read end), then X resumed: its lane issues them
    // at its next poll.
    releaseFifo(fifo);
    _ = linux.kill(rig.daemons.items[x].pid, .CONT);
    // The device's answer to X's released windows: the fence class latched
    // (the journal's reservation-conflict line, logged once at the latch).
    // X's `.stats` is no witness: the incarnation X learns is evicted holds
    // un-landed windows, so it is poisoned (decision 0003 §BS) and its
    // mount answers EIO.
    try rig.awaitLog(x, "SQUEEZEFS_TEST_LANDING_HOLD_RELEASE released", 30 * std.time.ns_per_s);
    try rig.awaitLog(x, "journal: reservation-conflict errno at", 30 * std.time.ns_per_s);
    try rig.kill9(x);
    try rig.umount(m);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

/// The data-fence row's data volume: 32 MiB at 64 KiB blocks (510 usable),
/// so a fill to ENOSPC takes every free block — the dead member's grant
/// window's among them.
const data_fence_volume: u64 = 32 << 20;

/// Open the FIFO's write end once its reader is there (`ENXIO` until then),
/// bounded: a held write whose process died never wedges the row.
fn releaseFifoBounded(path: [:0]const u8, deadline_ns: u64) !void {
    const deadline = clock.monotonicNs() + deadline_ns;
    while (true) {
        const rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .NONBLOCK = true, .CLOEXEC = true }, 0);
        switch (linux.errno(rc)) {
            .SUCCESS => {
                _ = linux.close(@intCast(rc));
                return;
            },
            .NXIO => {},
            else => return error.FifoRefused,
        }
        if (clock.monotonicNs() >= deadline) return error.Timeout;
        sleepMs(20);
    }
}

test "nvmet fidelity: a member's death fences its host on the DATA namespace before its extents are reallocated (decision 0003 §DJ) — X (host A, a joiner) writes one whole block, a block of its grant window, and the write is held past X's own gates (SQUEEZEFS_TEST_DATA_WRITE_HOLD); X is SIGSTOPped; M (host B) evicts and recovers it; M unmounts (the allocation lease's pages released: X's unpublished window is free for the next holder) and M2 (host B) mounts and fills the data volume to ENOSPC with 'G' blocks — X's window block among them; X resumed, its held write reaches the device and is refused (a reservation conflict), and every byte M2 wrote reads back exact from a fresh mount" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try daemonSetSized(rig, "df", transport, try rig.servicePorts(transport), &.{}, &.{ "--block-size", "65536" }, data_fence_volume);
    const cm = set.cm;
    var tb: [16]u8 = undefined;
    const t_owner = try std.fmt.bufPrint(&tb, "{d}", .{member_t_owner_ms});
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var fb: [128]u8 = undefined;
    const fifo = try std.fmt.bufPrintSentinel(&fb, "{s}/x-data-write.fifo", .{rig.root}, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.mknodat(linux.AT.FDCWD, fifo.ptr, linux.S.IFIFO | 0o600, 0)));
    var env_m = try rig.envWith(ident_b);
    defer env_m.deinit();
    try env_m.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    var env_x = try rig.envWithKnob("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    defer env_x.deinit();
    try env_x.put("SQUEEZEFS_TEST_DATA_WRITE_HOLD", fifo);
    const m = try rig.spawnMountEnv("df-m", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    const x = try rig.spawnMountEnv("df-x", set.metaUri(), &env_x, &.{});
    try rig.awaitLog(x, "ready: ", ready_wait_ns);
    {
        // Each host registered itself on the data namespace at its admission,
        // M reserving it (WEAR).
        const sm = try rig.stats(m);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "data_pr_registrations"));
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "data_pr_reservations"));
        const sx = try rig.stats(x);
        defer gpa.free(sx);
        try testing.expectEqual(@as(u64, 1), try statNumber(sx, "data_pr_registrations"));
        try testing.expectEqual(@as(u64, 0), try statNumber(sx, "data_pr_reservations"));
    }
    // X's first data write: one whole block of its own file, written through
    // to a block of its grant window — in the background (its DMA is held).
    try testing.expect(try scriptSucceeds(rig, x, "mkdir dx && (head -c 65536 /dev/zero | tr '\\000' X > dx/x) </dev/null >/dev/null 2>&1 &"));
    try rig.awaitLog(x, "SQUEEZEFS_TEST_DATA_WRITE_HOLD — data volume", ready_wait_ns);
    _ = linux.kill(rig.daemons.items[x].pid, .STOP);
    {
        const sm = try rig.awaitStat(m, "appender_recoveries", 1, ready_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "appender_recoveries"));
        // The recovery fenced host A on the data namespace (M's own client).
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "data_pr_preempts"));
        try testing.expectEqual(@as(u64, 0), try statNumber(sm, "data_pr_fence_failures"));
    }
    // M's clean unmount releases its allocation lease and the bitmap pages:
    // the next holder's bitmap is the durable references', X's window free.
    try rig.umount(m);
    const m2 = try rig.spawnMountEnv("df-m2", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m2, "ready: ", ready_wait_ns);
    {
        // M kept host B's registration (a preempt landed under it, the kept
        // law): M2 adopts; nothing to reconcile.
        const s2 = try rig.stats(m2);
        defer gpa.free(s2);
        try testing.expectEqual(@as(u64, 1), try statNumber(s2, "data_pr_adoptions"));
        try testing.expectEqual(@as(u64, 0), try statNumber(s2, "data_pr_reconcile_preempts"));
    }
    // The fill: one 'G' block per file, each synced, until the store answers
    // ENOSPC — every free block taken; the failed file goes.
    const filled = blk: {
        const out = try rig.onMount(m2, "i=0; while head -c 65536 /dev/zero | tr '\\000' G > g$i && sync -d g$i; do i=$((i+1)); done; rm -f g$i; echo $i");
        defer gpa.free(out);
        break :blk try std.fmt.parseInt(u64, std.mem.trim(u8, out, " \n"), 10);
    };
    try testing.expect(filled > 0);
    // X resumed: its held write reaches the device.
    _ = linux.kill(rig.daemons.items[x].pid, .CONT);
    try releaseFifoBounded(fifo, 30 * std.time.ns_per_s);
    try rig.awaitLog(x, "the held write at offset", 30 * std.time.ns_per_s);
    // Byte-exact from a fresh mount (no cache of M2's): every file 'G' whole.
    try rig.umount(m2);
    const m3 = try rig.spawnMountEnv("df-m3", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m3, "ready: ", ready_wait_ns);
    {
        const out = try rig.onMount(m3, "printf '%s %s\\n' \"$(cat g* | wc -c)\" \"$(cat g* | tr -d G | wc -c)\"");
        defer gpa.free(out);
        var it = std.mem.tokenizeAny(u8, out, " \n");
        const total = try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10);
        const foreign = try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10);
        try testing.expectEqual(@as(u64, 0), foreign);
        try testing.expectEqual(filled * 65536, total);
    }
    // The device refused X's write (EBADE: X's data device latched the fence
    // class).
    {
        const text = try rig.readLog(x);
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "the held write at offset") != null);
        try testing.expect(std.mem.indexOf(u8, text, "completed: refused") != null);
        try testing.expect(std.mem.indexOf(u8, text, "reservation-conflict errno class") != null);
    }
    try rig.kill9(x);
    try rig.umount(m3);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

test "nvmet fidelity: a PTPL-less target restart under a running set (review MAJOR 2 of decision 0003 §DJ) — tcp: M (host B) manages, J (host C) a live joiner, X (host A) frozen with a held data write and recovered by M (its host preempted on data); both subsystems wiped by hand and restored (nvmet keeps no PTPL: every registration and reservation forgotten) and the controllers reconnect; once M holds the metadata namespace again J writes and syncs — never fail-stopped —, M re-reserves the data namespace WEAR with every live host registered, X resumed is refused by the device, and J's bytes read back exact" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .tcp;
    const ports = try rig.servicePorts(transport);
    const set = try daemonSetSized(rig, "lp", transport, ports, &.{}, &.{ "--block-size", "65536" }, data_fence_volume);
    const cm = set.cm;
    var tb: [16]u8 = undefined;
    const t_owner = try std.fmt.bufPrint(&tb, "{d}", .{member_t_owner_ms});
    const cm_b = try rig.connect(set.meta_nqn, transport, ports[0], ident_b, &.{});
    const cm_c = try rig.connect(set.meta_nqn, transport, ports[0], ident_c, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var mu_c: [64]u8 = undefined;
    const meta_uri_c = try std.fmt.bufPrint(&mu_c, "sqmeta://{s}", .{cm_c.dev()});
    var fb: [128]u8 = undefined;
    const fifo = try std.fmt.bufPrintSentinel(&fb, "{s}/x-lapse-write.fifo", .{rig.root}, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.mknodat(linux.AT.FDCWD, fifo.ptr, linux.S.IFIFO | 0o600, 0)));
    var env_m = try rig.envWith(ident_b);
    defer env_m.deinit();
    try env_m.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    var env_j = try rig.envWith(ident_c);
    defer env_j.deinit();
    try env_j.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    var env_x = try rig.envWithKnob("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", t_owner);
    defer env_x.deinit();
    try env_x.put("SQUEEZEFS_TEST_DATA_WRITE_HOLD", fifo);
    const m = try rig.spawnMountEnv("lp-m", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    const j = try rig.spawnMountEnv("lp-j", meta_uri_c, &env_j, &.{});
    try rig.awaitLog(j, "ready: ", ready_wait_ns);
    const x = try rig.spawnMountEnv("lp-x", set.metaUri(), &env_x, &.{});
    try rig.awaitLog(x, "ready: ", ready_wait_ns);
    // X's first data write, held past its own gates; X frozen and recovered.
    try testing.expect(try scriptSucceeds(rig, x, "mkdir dx && (head -c 65536 /dev/zero | tr '\\000' X > dx/x) </dev/null >/dev/null 2>&1 &"));
    try rig.awaitLog(x, "SQUEEZEFS_TEST_DATA_WRITE_HOLD — data volume", ready_wait_ns);
    _ = linux.kill(rig.daemons.items[x].pid, .STOP);
    {
        const sm = try rig.awaitStat(m, "appender_recoveries", 1, ready_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "data_pr_preempts"));
    }
    // The target restart: both subsystems' objects removed through configfs
    // (marker-attested, the ledger untouched) and rebuilt by `restore` — the
    // namespaces enabled afresh with no registrant and no reservation.
    for ([_][]const u8{ set.meta_nqn, set.data_nqn }) |nqn| {
        try testing.expect(configfs.isHarnessNqn(nqn));
        var rec = (try rig.ledgerRecord(nqn)).?;
        defer rec.deinit(gpa);
        var ids: [ledger.MAX_LISTENERS]u16 = undefined;
        _ = try configfs.teardown(configfs.RealFs.face(), gpa, .{ .subnqn = nqn, .port_ids = rec.portIds(&ids), .allow_hosts = &.{}, .host_named_elsewhere = struct {
            fn f(_: *const anyopaque, _: []const u8) bool {
                return true;
            }
        }.f, .ctx = @ptrCast(&rig) });
    }
    const ro = try rig.sqzOk(&.{ "nvmeof", "restore" });
    defer gpa.free(ro);
    try testing.expect(std.mem.indexOf(u8, ro, ": restored") != null);
    // The controllers reconnect and M holds the metadata namespace again.
    {
        const sm = try rig.awaitStat(m, "writer_guard_pr_reacquires", 1, g2_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 0), try statNumber(sm, "writer_guard_fenced"));
    }
    // J was never fail-stopped: it writes and syncs under M's hold.
    try testing.expect(try scriptSucceeds(rig, j, "head -c 262144 /dev/zero | tr '\\000' L > jl && sync -f . && sync -d jl"));
    // M re-reserved the data namespace (every live host re-registered first).
    {
        const sm = try rig.awaitStat(m, "data_pr_lapse_rereservations", 1, g2_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 0), try statNumber(sm, "data_pr_fence_failures"));
        const sj = try rig.stats(j);
        defer gpa.free(sj);
        try testing.expect(try statNumber(sj, "data_pr_lapse_reregistrations") >= 1);
        try testing.expect(try statNumber(sj, "meta_pr_lapse_reregistrations") >= 1);
    }
    // X resumed: its held write reaches the device and is refused.
    _ = linux.kill(rig.daemons.items[x].pid, .CONT);
    try releaseFifoBounded(fifo, 30 * std.time.ns_per_s);
    try rig.awaitLog(x, "the held write at offset", 30 * std.time.ns_per_s);
    {
        const text = try rig.readLog(x);
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "completed: refused") != null);
        try testing.expect(std.mem.indexOf(u8, text, "reservation-conflict errno class") != null);
    }
    // J's bytes, exact, and J writes on.
    {
        const out = try rig.onMount(j, "printf '%s %s\\n' \"$(wc -c < jl)\" \"$(tr -d L < jl | wc -c)\"; head -c 65536 /dev/zero | tr '\\000' K > jk && sync -d jk");
        defer gpa.free(out);
        var it = std.mem.tokenizeAny(u8, out, " \n");
        try testing.expectEqual(@as(u64, 262144), try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10));
        try testing.expectEqual(@as(u64, 0), try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10));
    }
    try rig.kill9(x);
    try rig.umount(j);
    try rig.umount(m);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_c);
    try rig.disconnect(&cm_c);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

/// A PTPL-less target restart of both subsystems: their configfs objects
/// removed (marker-attested, the ledger untouched) and rebuilt by `restore`
/// — the namespaces enabled afresh with no registrant and no reservation.
fn restartTarget(rig: *Rig, set: anytype) !void {
    return restartSubsystems(rig, &.{ set.meta_nqn, set.data_nqn });
}

/// The same restart of the named subsystems only (one target of several).
fn restartSubsystems(rig: *Rig, nqns: []const []const u8) !void {
    const gpa = rig.gpa;
    for (nqns) |nqn| {
        try testing.expect(configfs.isHarnessNqn(nqn));
        var rec = (try rig.ledgerRecord(nqn)).?;
        defer rec.deinit(gpa);
        var ids: [ledger.MAX_LISTENERS]u16 = undefined;
        _ = try configfs.teardown(configfs.RealFs.face(), gpa, .{ .subnqn = nqn, .port_ids = rec.portIds(&ids), .allow_hosts = &.{}, .host_named_elsewhere = struct {
            fn f(_: *const anyopaque, _: []const u8) bool {
                return true;
            }
        }.f, .ctx = @ptrCast(rig) });
    }
    const ro = try rig.sqzOk(&.{ "nvmeof", "restore" });
    defer gpa.free(ro);
    try testing.expect(std.mem.indexOf(u8, ro, ": restored") != null);
}

/// The beats a lapse took, read off the manager's `.stats` (the evidence's
/// window, counted in beats, never in wall time).
fn printLapseBeats(rig: *Rig, d: usize, what: []const u8) !void {
    const sm = try rig.stats(d);
    defer rig.gpa.free(sm);
    std.debug.print("\n{s}: writer_guard_lapse_beats {?d}, writer_guard_lapse_early_ends {?d}, data_pr_lapse_beats {?d}, data_pr_lapse_early_rereservations {?d}, data_pr_lapse_kicks {?d}\n", .{ what, json_scan.number(sm, "writer_guard_lapse_beats"), json_scan.number(sm, "writer_guard_lapse_early_ends"), json_scan.number(sm, "data_pr_lapse_beats"), json_scan.number(sm, "data_pr_lapse_early_rereservations"), json_scan.number(sm, "data_pr_lapse_kicks") });
}

test "nvmet fidelity: the lapse grace ends early once every live host is back (decision 0003 §DL) — tcp: M (host B) manages under a lapse grace far past the row's bound (SQUEEZEFS_TEST_LAPSE_GRACE_S 600), J (host C) a live joiner; both subsystems wiped and restored (nvmet keeps no PTPL); J's beat re-registers its host on both namespaces, and M takes the metadata hold back as soon as every live member's host key in the census is registered there (writer_guard_lapse_early_ends 1, never the grace) and re-reserves the data namespace WEAR at once (data_pr_lapse_early_rereservations 1); J writes and syncs under both; the lapse's beats printed" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .tcp;
    const ports = try rig.servicePorts(transport);
    const set = try daemonSetSized(rig, "le", transport, ports, &.{}, &.{ "--block-size", "65536" }, data_fence_volume);
    const cm = set.cm;
    const cm_b = try rig.connect(set.meta_nqn, transport, ports[0], ident_b, &.{});
    const cm_c = try rig.connect(set.meta_nqn, transport, ports[0], ident_c, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var mu_c: [64]u8 = undefined;
    const meta_uri_c = try std.fmt.bufPrint(&mu_c, "sqmeta://{s}", .{cm_c.dev()});
    var env_m = try rig.envWith(ident_b);
    defer env_m.deinit();
    try env_m.put("SQUEEZEFS_TEST_LAPSE_GRACE_S", "600");
    var env_j = try rig.envWith(ident_c);
    defer env_j.deinit();
    const m = try rig.spawnMountEnv("le-m", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    const j = try rig.spawnMountEnv("le-j", meta_uri_c, &env_j, &.{});
    try rig.awaitLog(j, "ready: ", ready_wait_ns);
    try testing.expect(try scriptSucceeds(rig, j, "head -c 65536 /dev/zero | tr '\\000' P > jp && sync -d jp"));
    try restartTarget(rig, set);
    // The controllers reconnect; J's beat re-registers its host; M's hold
    // returns long before its 600 s grace.
    {
        const sm = try rig.awaitStat(m, "writer_guard_pr_reacquires", 1, g2_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "writer_guard_pr_reacquires"));
        try testing.expectEqual(@as(u64, 0), try statNumber(sm, "writer_guard_fenced"));
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "writer_guard_lapse_early_ends"));
    }
    {
        const sm = try rig.awaitStat(m, "data_pr_lapse_rereservations", 1, g2_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "data_pr_lapse_rereservations"));
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "data_pr_lapse_early_rereservations"));
        const sj = try rig.stats(j);
        defer gpa.free(sj);
        try testing.expect(try statNumber(sj, "meta_pr_lapse_reregistrations") >= 1);
        try testing.expect(try statNumber(sj, "data_pr_lapse_reregistrations") >= 1);
    }
    try printLapseBeats(rig, m, "the lapse grace's early end (M)");
    // J writes and syncs under the returned hold and reservation.
    {
        const out = try rig.onMount(j, "head -c 262144 /dev/zero | tr '\\000' L > jl && sync -d jl && printf '%s %s\\n' \"$(wc -c < jl)\" \"$(tr -d L < jl | wc -c)\"");
        defer gpa.free(out);
        var it = std.mem.tokenizeAny(u8, out, " \n");
        try testing.expectEqual(@as(u64, 262144), try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10));
        try testing.expectEqual(@as(u64, 0), try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10));
    }
    try rig.umount(j);
    try rig.umount(m);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_c);
    try rig.disconnect(&cm_c);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

test "nvmet fidelity: a member slow on data past the lapse's re-reservation is re-admitted at its refused write, never fail-stopped (decision 0003 §DL) — tcp: M (host B) manages, J (host C) a live joiner whose first lapse beat parks after its metadata re-registration (SQUEEZEFS_TEST_LAPSE_BEAT_HOLD); both subsystems wiped and restored; M takes the hold back and re-reserves the data namespace once the grace passed without J; J's write and sync meet the reservation conflict, and J — registered on the metadata namespace, its page Live, inside the lapse — re-registers its host through the rule and retries (data_pr_refusal_readmissions ≥ 1, no fence latched); its bytes read back exact and it writes on after its beat resumes" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .tcp;
    const ports = try rig.servicePorts(transport);
    const set = try daemonSetSized(rig, "ls", transport, ports, &.{}, &.{ "--block-size", "65536" }, data_fence_volume);
    const cm = set.cm;
    const cm_b = try rig.connect(set.meta_nqn, transport, ports[0], ident_b, &.{});
    const cm_c = try rig.connect(set.meta_nqn, transport, ports[0], ident_c, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    var mu_c: [64]u8 = undefined;
    const meta_uri_c = try std.fmt.bufPrint(&mu_c, "sqmeta://{s}", .{cm_c.dev()});
    var fb: [128]u8 = undefined;
    const fifo = try std.fmt.bufPrintSentinel(&fb, "{s}/j-lapse-beat.fifo", .{rig.root}, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.mknodat(linux.AT.FDCWD, fifo.ptr, linux.S.IFIFO | 0o600, 0)));
    var env_m = try rig.envWith(ident_b);
    defer env_m.deinit();
    var env_j = try rig.envWith(ident_c);
    defer env_j.deinit();
    try env_j.put("SQUEEZEFS_TEST_LAPSE_BEAT_HOLD", fifo);
    const m = try rig.spawnMountEnv("ls-m", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    const j = try rig.spawnMountEnv("ls-j", meta_uri_c, &env_j, &.{});
    try rig.awaitLog(j, "ready: ", ready_wait_ns);
    try testing.expect(try scriptSucceeds(rig, j, "head -c 65536 /dev/zero | tr '\\000' P > jp && sync -d jp"));
    try restartTarget(rig, set);
    // J's beat re-registers its host on the metadata namespace, then parks
    // before its data namespace; M re-reserves without it once the grace
    // passed.
    try rig.awaitLog(j, "SQUEEZEFS_TEST_LAPSE_BEAT_HOLD — the lapse beat parks", g2_wait_ns);
    {
        const sm = try rig.awaitStat(m, "data_pr_lapse_rereservations", 1, g2_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "data_pr_lapse_rereservations"));
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "writer_guard_pr_reacquires"));
    }
    // J writes and syncs: its first data write meets the conflict, J is
    // re-admitted and the write retried.
    try testing.expect(try scriptSucceeds(rig, j, "head -c 262144 /dev/zero | tr '\\000' S > js && sync -d js"));
    {
        const sj = try rig.stats(j);
        defer gpa.free(sj);
        try testing.expect(try statNumber(sj, "data_pr_write_refusals") >= 1);
        try testing.expect(try statNumber(sj, "data_pr_refusal_readmissions") >= 1);
        try testing.expectEqual(@as(u64, 0), try statNumber(sj, "data_dma_fence_refusals"));
    }
    try releaseFifoBounded(fifo, 30 * std.time.ns_per_s);
    try rig.awaitLog(j, "SQUEEZEFS_TEST_LAPSE_BEAT_HOLD — the lapse beat resumes", 30 * std.time.ns_per_s);
    {
        const out = try rig.onMount(j, "printf '%s %s\\n' \"$(wc -c < js)\" \"$(tr -d S < js | wc -c)\"; head -c 65536 /dev/zero | tr '\\000' K > jk && sync -d jk");
        defer gpa.free(out);
        var it = std.mem.tokenizeAny(u8, out, " \n");
        try testing.expectEqual(@as(u64, 262144), try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10));
        try testing.expectEqual(@as(u64, 0), try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10));
    }
    try rig.umount(j);
    try rig.umount(m);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_c);
    try rig.disconnect(&cm_c);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

/// A symmetric set of TWO metadata volumes and one data volume on nvmet-tcp,
/// each subsystem on a port of its own: formatted over the rig's connects
/// (`sqmeta://<meta0>,<meta1>`), the data volume's endpoint record set (the
/// daemon's own connect), its formatting connect released.
const TwoMetaSet = struct {
    meta_nqn: [2][]const u8,
    data_nqn: []const u8,
    ports: [3]u16,
    cm: [2]Connected,
    uri_buf: [160]u8 = undefined,
    uri_len: usize = 0,

    fn metaUri(self: *const TwoMetaSet) []const u8 {
        return self.uri_buf[0..self.uri_len];
    }

    /// Both metadata subsystems connected under `ident`: the set's URI for
    /// a mount of that host (`buf` holds it).
    fn connectHost(self: *const TwoMetaSet, rig: *Rig, ident: initiator.HostPair, out: *[2]Connected, buf: []u8) ![]const u8 {
        for (0..2) |v| out[v] = try rig.connect(self.meta_nqn[v], .tcp, self.ports[v], ident, &.{});
        return std.fmt.bufPrint(buf, "sqmeta://{s},{s}", .{ out[0].dev(), out[1].dev() });
    }
};

fn twoMetaSet(rig: *Rig, prefix: []const u8, data_size: u64) !TwoMetaSet {
    const ports: [3]u16 = .{ try rig.tcpPort(), try rig.tcpPort(), try rig.tcpPort() };
    var nb: [64]u8 = undefined;
    const m0 = try rig.share(try std.fmt.bufPrint(&nb, "{s}meta0", .{prefix}), meta_len, .tcp, ports[0]);
    const m1 = try rig.share(try std.fmt.bufPrint(&nb, "{s}meta1", .{prefix}), meta_len, .tcp, ports[1]);
    const d = try rig.share(try std.fmt.bufPrint(&nb, "{s}data", .{prefix}), data_size, .tcp, ports[2]);
    var out: TwoMetaSet = .{ .meta_nqn = .{ m0, m1 }, .data_nqn = d, .ports = ports, .cm = undefined };
    out.cm[0] = try rig.connect(m0, .tcp, ports[0], null, &.{});
    out.cm[1] = try rig.connect(m1, .tcp, ports[1], null, &.{});
    const cd = try rig.connect(d, .tcp, ports[2], null, &.{});
    out.uri_len = (try std.fmt.bufPrint(&out.uri_buf, "sqmeta://{s},{s}", .{ out.cm[0].dev(), out.cm[1].dev() })).len;
    var du: [64]u8 = undefined;
    const fmt_out = try rig.sqzOk(&.{ "format", "--block-size", "65536", out.metaUri(), try std.fmt.bufPrint(&du, "sqdata://{s}", .{cd.dev()}) });
    defer rig.gpa.free(fmt_out);
    try testing.expect(std.mem.indexOf(u8, fmt_out, "data volumes 1") != null);
    try rig.setDataEndpoint(out.metaUri(), .tcp, ports[2], d);
    try rig.disconnect(&cd);
    return out;
}

test "nvmet fidelity: a restart of ANOTHER metadata volume's target than volume 0's never fail-stops a live joiner (decision 0003 §DN, §DL.8(b)) — tcp, a symmetric set of two metadata volumes: M (host B) manages both, J (host C) a live joiner of both, its directories spread over both members; only volume 1's metadata subsystem wiped and restored (nvmet keeps no PTPL: its registrations and its reservation forgotten, volume 0's and the data namespace's untouched); J's lapse beat re-registers its host on volume 1's namespace and volume 1's guard takes its hold back as soon as every live member's host key is registered THERE (writer_guard_lapse_early_ends 1, inside a grace of 60 s); J writes and syncs into every directory under the returned hold — never fail-stopped — and its bytes read back exact" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const set = try twoMetaSet(rig, "mv", data_fence_volume);
    var cm_b: [2]Connected = undefined;
    var ub: [160]u8 = undefined;
    const meta_uri_b = try set.connectHost(rig, ident_b, &cm_b, &ub);
    var cm_c: [2]Connected = undefined;
    var uc: [160]u8 = undefined;
    const meta_uri_c = try set.connectHost(rig, ident_c, &cm_c, &uc);
    var env_m = try rig.envWith(ident_b);
    defer env_m.deinit();
    // A grace the row's bound covers: without the joiner's beat on volume 1
    // the hold returns after it and the joiner's next write there is refused.
    try env_m.put("SQUEEZEFS_TEST_LAPSE_GRACE_S", "60");
    var env_j = try rig.envWith(ident_c);
    defer env_j.deinit();
    const m = try rig.spawnMountEnv("mv-m", meta_uri_b, &env_m, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    const j = try rig.spawnMountEnv("mv-j", meta_uri_c, &env_j, &.{});
    try rig.awaitLog(j, "ready: ", ready_wait_ns);
    // Root children spread round-robin over the members (§BZ): J's
    // directories land on both volumes, their files beside them.
    try testing.expect(try scriptSucceeds(rig, j, "for d in d0 d1 d2 d3; do mkdir $d && head -c 65536 /dev/zero | tr '\\000' P > $d/p; done && sync -f ."));
    {
        const sj = try rig.stats(j);
        defer gpa.free(sj);
        try testing.expect(try statNumber(sj, "routing_dir_place_remote") >= 1);
    }
    try restartSubsystems(rig, &.{set.meta_nqn[1]});
    // The controllers reconnect; volume 1's guard holds again.
    {
        const sm = try rig.awaitStat(m, "writer_guard_pr_reacquires", 1, g2_wait_ns);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "writer_guard_pr_reacquires"));
        try testing.expectEqual(@as(u64, 0), try statNumber(sm, "writer_guard_fenced"));
    }
    // J was never fail-stopped: it writes and syncs into every directory —
    // those on volume 1 included — under the returned hold.
    if (!try scriptSucceeds(rig, j, "for d in d0 d1 d2 d3; do head -c 262144 /dev/zero | tr '\\000' L > $d/l; done && sync -f .")) {
        rig.printLogTail(j, 40);
        return error.JoinerFailStopped;
    }
    {
        const sm = try rig.stats(m);
        defer gpa.free(sm);
        try testing.expectEqual(@as(u64, 1), try statNumber(sm, "writer_guard_lapse_early_ends"));
        // The data namespace never lapsed.
        try testing.expectEqual(@as(u64, 0), try statNumber(sm, "data_pr_lapses"));
        const sj = try rig.stats(j);
        defer gpa.free(sj);
        try testing.expect(try statNumber(sj, "meta_pr_lapse_reregistrations") >= 1);
        try testing.expectEqual(@as(u64, 0), try statNumber(sj, "data_pr_lapse_reregistrations"));
    }
    try printLapseBeats(rig, m, "a restart of volume 1's metadata target (M)");
    {
        const out = try rig.onMount(j, "cat d0/l d1/l d2/l d3/l | wc -c; cat d0/l d1/l d2/l d3/l | tr -d L | wc -c; head -c 65536 /dev/zero | tr '\\000' K > d1/k && sync -f .");
        defer gpa.free(out);
        var it = std.mem.tokenizeAny(u8, out, " \n");
        try testing.expectEqual(@as(u64, 4 * 262144), try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10));
        try testing.expectEqual(@as(u64, 0), try std.fmt.parseInt(u64, it.next() orelse return error.ScriptFailed, 10));
    }
    try rig.umount(j);
    try rig.umount(m);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_c);
    for (0..2) |v| {
        try rig.disconnect(&cm_c[v]);
        try rig.disconnect(&cm_b[v]);
        try rig.disconnect(&set.cm[v]);
    }
    try rig.unshare(set.data_nqn);
    for (set.meta_nqn) |nqn| try rig.unshare(nqn);
    try expectNoResidue(rig, set.data_nqn);
    for (set.meta_nqn) |nqn| try expectNoResidue(rig, nqn);
}

/// The manager's fencing registrant's controller of `nqn`: the one whose
/// identity none of the rig's connects carries.
fn fencerController(rig: *Rig, nqn: []const u8) !?initiator.Controller {
    const ctrls = try initiator.listControllers(rig.gpa, initiator.SYSFS_ROOT);
    defer rig.gpa.free(ctrls);
    for (ctrls) |c| {
        if (!std.mem.eql(u8, c.subsysnqnOf(), nqn)) continue;
        var ours = false;
        for ([_]initiator.HostPair{ ident_a, ident_b, ident_c }) |i| ours = ours or c.hasIdentity(i.hostnqn, i.hostid);
        if (!ours) return c;
    }
    return null;
}

test "nvmet fidelity: a metadata mount pointed at the fencing registrant's namespace node is refused by name (review M3 of decision 0003 §DG) — M (host B) manages a symmetric set and stood its fencer; a mount with no explicit pair on the fencer's block node (a by-id link that picked it, a hand-typed path) never registers there: it exits naming the fencer; `appender clear` on that node is refused the same way (review minor 1: the refusal sits at the metadata door every user passes); the fencer's registration under M's key stands (no own-stale ladder unregistered it)" {
    const gpa = testing.allocator;
    var rig = try venue(gpa, testing.io);
    defer rig.deinit();
    const transport: configfs.Trtype = .loop;
    const set = try symmetricSet(rig, "fz", transport, try rig.servicePorts(transport));
    const cm = set.cm;
    const cm_b = try rig.connect(set.meta_nqn, transport, 0, ident_b, &.{});
    var mu_b: [64]u8 = undefined;
    const meta_uri_b = try std.fmt.bufPrint(&mu_b, "sqmeta://{s}", .{cm_b.dev()});
    const m = try rig.spawnMount("fz-m", meta_uri_b, ident_b, &.{});
    try rig.awaitLog(m, "ready: ", ready_wait_ns);
    const key_m = try statString(rig, m, "pr_key");
    defer gpa.free(key_m);
    const fc = (try fencerController(rig, set.meta_nqn)) orelse return error.NoFencerController;
    const ns = (try initiator.controllerNamespace(gpa, initiator.SYSFS_ROOT, fc.instance)) orelse return error.NoFencerNamespace;
    var nb: [40]u8 = undefined;
    var ub: [64]u8 = undefined;
    const meta_uri_f = try std.fmt.bufPrint(&ub, "sqmeta://{s}", .{ns.blockPath(&nb)});
    var env_z = try rig.envWithKnob("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", "3000");
    defer env_z.deinit();
    _ = env_z.swapRemove(hostnqn_knob);
    _ = env_z.swapRemove(hostid_knob);
    const z = try rig.spawnMountEnv("fz-z", meta_uri_f, &env_z, &.{});
    try testing.expect(!(try readyOrDead(rig, z)));
    {
        // The refusal's tail: the CLI's own stdout line is written at the
        // log's offset 0 over the start of the stderr text that preceded it.
        const text = try rig.readLog(z);
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "use the host's own node of the namespace (decision 0003 §DG.16)") != null);
    }
    {
        // `appender clear` on the fencer's node: refused at the metadata
        // door every user passes (review minor 1), never writing through the
        // fencer's controller.
        const r = try rig.sqzEnv(&.{ "appender", "clear", meta_uri_f, "1" }, &env_z);
        defer rig.freeRun(r);
        try testing.expect(!r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stderr, "fencing registrant") != null);
    }
    {
        // The fencer still registered under M's key beside M's host.
        const rr = try rig.resvReportJson(cm.dev());
        defer gpa.free(rr);
        try testing.expectEqual(@as(usize, 3), std.mem.count(u8, rr, key_m));
        try testing.expectEqual(@as(?u64, 2), json_scan.number(rr, "regctl"));
    }
    try rig.umount(m);
    // Whatever the refused mount connected on the data subsystem is the rig's
    // to sweep (its identity is the host's derived one).
    for ([_][]const u8{ set.data_nqn, set.meta_nqn }) |nqn| {
        const ctrls = try initiator.listControllers(gpa, initiator.SYSFS_ROOT);
        defer gpa.free(ctrls);
        for (ctrls) |c| if (std.mem.eql(u8, c.subsysnqnOf(), nqn) and !c.hasIdentity(ident_a.hostnqn, ident_a.hostid) and !c.hasIdentity(ident_b.hostnqn, ident_b.hostid)) {
            initiator.deleteController(c.instance) catch {};
            _ = initiator.awaitControllerGone(c.instance);
        };
    }
    _ = try rig.disconnectIdentity(set.data_nqn, ident_a);
    _ = try rig.disconnectIdentity(set.data_nqn, ident_b);
    try rig.disconnect(&cm_b);
    try rig.disconnect(&cm);
    try rig.unshare(set.data_nqn);
    try rig.unshare(set.meta_nqn);
    try expectNoResidue(rig, set.meta_nqn);
    try expectNoResidue(rig, set.data_nqn);
}

/// A stats string that must be present, owned by the caller.
fn statString(rig: *Rig, d: usize, key: []const u8) ![]u8 {
    const st = try rig.stats(d);
    defer rig.gpa.free(st);
    const v = json_scan.string(st, key) orelse {
        std.debug.print("\n--- .stats carries no \"{s}\" ---\n", .{key});
        return error.StatMissing;
    };
    return rig.gpa.dupe(u8, v);
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
    // A tcp listener: a loop connect without an address reaches only the
    // FIRST loop port on the host, so a hand-built address-less loop port
    // would be unreachable beside any other (a concurrent run's); tcp
    // addresses the port by ip:port.
    const hand_port = try rig.tcpPort();
    var hl = try configfs.Listener.tcp("127.0.0.1", hand_port);
    hl.port_id = try smallPortId(fs, rig.pid);
    var hand_dir_b: [32]u8 = undefined;
    const hand_dir = try std.fmt.bufPrint(&hand_dir_b, "ports/{d}", .{hl.port_id});
    // The run's own namespace uuid: the host's nvme core ignores a
    // namespace whose uuid another subsystem already shows (a concurrent
    // run's hand share), so the pid rides its last four bytes.
    var hand_uuid: [16]u8 = .{ 0x7a, 0x7a, 0x7a, 0x7a, 0x11, 0x11, 0x41, 0x11, 0x81, 0x11, 1, 2, 0, 0, 0, 0 };
    std.mem.writeInt(u32, hand_uuid[12..16], @intCast(rig.pid), .big);
    _ = try configfs.apply(fs, .{ .subnqn = hand, .backing_path = backing, .ns_uuid = hand_uuid, .allow_hosts = &.{}, .listeners = &.{hl} });
    const hand_owned = try gpa.dupe(u8, hand);
    try rig.shares.append(gpa, hand_owned);
    const c = try rig.connect(hand, .tcp, hand_port, null, &.{});
    const lj = try rig.listJson();
    defer gpa.free(lj);
    try testing.expect(foreignLiveNames(lj, hand));
    var before = try configfs.walkLive(fs, gpa);
    defer before.deinit(gpa);
    const ao = try rig.sqzOk(&.{ "nvmeof", "adopt", hand });
    defer gpa.free(ao);
    try testing.expect(std.mem.indexOf(u8, ao, "adopted ") != null and std.mem.indexOf(u8, ao, "foreign") != null);
    var after = try configfs.walkLive(fs, gpa);
    defer after.deinit(gpa);
    // configfs untouched: the share and its port read as before.
    const hs_before = before.find(hand).?;
    const hs_after = after.find(hand).?;
    try testing.expectEqual(hs_before.listeners.len, hs_after.listeners.len);
    try testing.expectEqual(hs_before.listeners[0].port_id, hs_after.listeners[0].port_id);
    try testing.expectEqualSlices(u32, hs_before.nsids, hs_after.nsids);
    try testing.expect(before.ports.find(hl.port_id) != null and after.ports.find(hl.port_id) != null);
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
    try testing.expect(foreignLiveNames(lj2, own));
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
    // the PRODUCT range (a harness port id refuses adoption): share it
    // with the product base, lose the ledger, adopt, restore, unshare.
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
