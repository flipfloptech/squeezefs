//! The daemon-owned data-plane connect at `mount` and its release at the
//! clean `umount` (charter §8 "NVMe-oF targets"; `docs/porting/
//! ops_nvmet_keys.md` §3.7 KD-MW-15; the rung-8a3 owed row). The set
//! config's `FabricEndpoint` record per data volume is the operator's
//! declaration that THIS daemon owns the volume's fabric association: at
//! mount, before discovery opens any data device, the daemon resolves its
//! own host identity (the explicit pair → `/etc/nvme` both → the
//! machine-id derivation SALTED BY THE MOUNTPOINT, so co-located mounts
//! never share a controller and a remount at the same mountpoint presents
//! the same identity — the reservation's identity stamp never reads a
//! change), reuses a controller already riding that identity for the
//! record's subsystem (a crashed predecessor's association) or connects
//! one (`initiator.connect`: the uevent-driven readiness wait), probes the
//! node's DMA grain (`align_probe`) and hands discovery the LIVE node path
//! — the data superblock read right after is the identity law (`vol_tag`,
//! `device_uuid`, `set_uuid`): a wrong namespace refuses by name and the
//! association is released with the mount.
//!
//! The §5.2 ladder (`initiator.fabricLadderVerdict`) judges every other
//! volume: with an explicit pair a META volume's controller must carry
//! the pair (rule 2) and a DATA volume without a record refuses UNIFORMLY
//! naming `config set-fabric-endpoints`; with no explicit pair everything
//! accepts unchanged. The one departure from the reference (decision 0002
//! row 486): a record-covered data volume is a daemon connect UNDER THE
//! RESOLVED identity even with no explicit pair — the record IS the
//! operator's intent, and opening the recorded path "as-is" would bind a
//! renumbered node.
//!
//! Ownership = identity (the reference's `--identity-only` law): at the
//! teardown, after every device fd closed, the daemon deletes exactly the
//! controllers of its records' subsystems that carry ITS pair — a
//! co-located mount's (another pair) or the operator's (the host default)
//! association is never touched, and `/sys/class/nvme` reads as the mount
//! found it.
//!
//! MULTIPATH (`multipath.zig`): a record names one subsystem over SEVERAL
//! paths (several Ethernet links). The daemon connects EVERY path under
//! the one identity (a controller already riding it on the path's address
//! is reused), so the kernel's native multipath merges them into one head,
//! which is handed to discovery only when the head's serving set is
//! EXACTLY ours (every path ours, no foreign one). A path that fails while
//! a sibling serves is a LOUD warning + `fabric_paths_degraded` — the mount
//! runs on the paths it has; zero paths up refuses as before. The kernel's
//! `iopolicy` default (`numa`) sends every I/O of a NUMA node down ONE
//! path, so the mount writes `SQUEEZEFS_FABRIC_IOPOLICY` (default
//! `round-robin`: every stream uses every link — owner ruling 2026-10-18)
//! to every owned subsystem and reads it back — a mismatch is a warning +
//! `fabric_iopolicy_failures`, never a refusal.
//! `fabric_paths_live` sums our serving controllers over the owned
//! subsystems. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const align_probe = @import("../../io/align_probe.zig");
const pr = @import("../../meta/guard/pr.zig");
const proc = @import("../../meta/guard/proc.zig");
const set_config = @import("../../data/set_config.zig");
const staging = @import("../../data/staging.zig");
const initiator = @import("initiator.zig");
const multipath = @import("multipath.zig");
const configfs = @import("configfs.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/mount_fabric is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// One path of a bound volume: the record's path and the controller the
/// connect (or the reuse) resolved for it.
pub const BoundPath = struct {
    path: set_config.FabricPath,
    instance: u32,
    reused: bool,
};

/// One association this mount owns: the record's subsystem, the paths
/// that came up (≥ 1), the head (or the per-controller node) handed to
/// discovery, the subsystem dir and the iopolicy it reads back.
pub const Bound = struct {
    vol_tag: u64,
    subnqn_buf: [configfs.NVMF_NQN_SIZE]u8 = undefined,
    subnqn_len: usize = 0,
    device_buf: [40]u8 = undefined,
    device_len: usize = 0,
    paths: [set_config.MAX_FABRIC_PATHS]BoundPath = undefined,
    n_paths: usize = 0,
    /// Paths of the record that did not come up (the mount runs degraded).
    paths_failed: usize = 0,
    /// `nvme-subsys<K>`; empty on a kernel without native multipath.
    subsys_buf: [32]u8 = undefined,
    subsys_len: usize = 0,
    /// The iopolicy read back after the write; empty when none applied.
    iopolicy_buf: [32]u8 = undefined,
    iopolicy_len: usize = 0,

    pub fn subnqn(self: *const Bound) []const u8 {
        return self.subnqn_buf[0..self.subnqn_len];
    }
    pub fn device(self: *const Bound) []const u8 {
        return self.device_buf[0..self.device_len];
    }
    pub fn subsys(self: *const Bound) []const u8 {
        return self.subsys_buf[0..self.subsys_len];
    }
    pub fn iopolicy(self: *const Bound) []const u8 {
        return self.iopolicy_buf[0..self.iopolicy_len];
    }
    pub fn pathsUp(self: *const Bound) []const BoundPath {
        return self.paths[0..self.n_paths];
    }

    fn instances(self: *const Bound, out: *[set_config.MAX_FABRIC_PATHS]u32) []const u32 {
        for (self.pathsUp(), out[0..self.n_paths]) |p, *o| o.* = p.instance;
        return out[0..self.n_paths];
    }
};

/// Why the mount refused (the operator text names the volume and the
/// ladder's remedy).
pub const Refusal = union(enum) {
    identity: initiator.IdentityRefusal,
    ladder: struct { vol_tag: u64, path: []const u8, verdict: initiator.Verdict },
    /// No path of the record came up; the last failure named.
    connect: struct { vol_tag: u64, subnqn: []const u8, err: initiator.ConnectError, spec: initiator.ConnectSpec, paths: usize },
    /// The head's serving set is not exactly ours, or several paths met a
    /// kernel that makes no head.
    head: struct { vol_tag: u64, subnqn: []const u8, verdict: multipath.HeadVerdict },
    /// The connected node's DMA grain is past what the planes admit.
    grain: struct { vol_tag: u64, path: []const u8, grain: u64 },
    out_of_memory,

    pub fn write(self: Refusal, w: *Io.Writer) Io.Writer.Error!void {
        switch (self) {
            .identity => |r| {
                try w.writeAll("fabric host identity: ");
                try r.write(w);
            },
            .ladder => |l| {
                if (l.vol_tag != 0) try w.print("data volume vol-{x:0>16} ({s}): ", .{ l.vol_tag, l.path }) else try w.print("metadata volume {s}: ", .{l.path});
                try l.verdict.write(w);
            },
            .connect => |c| {
                try w.print("data volume vol-{x:0>16}: none of the {d} path(s) of {s} came up; the last: ", .{ c.vol_tag, c.paths, c.subnqn });
                try initiator.writeConnectRefusal(w, c.err, c.spec);
            },
            .head => |h| {
                try w.print("data volume vol-{x:0>16}: the multipath head of {s} is refused: ", .{ h.vol_tag, h.subnqn });
                try h.verdict.write(w);
            },
            .grain => |g| try w.print("data volume vol-{x:0>16}: the connected namespace {s} reports a DMA grain of {d} bytes, past the {d} the data plane admits", .{ g.vol_tag, g.path, g.grain, align_probe.max_grain }),
            .out_of_memory => try w.writeAll("out of memory binding the fabric endpoints"),
        }
    }
};

/// A METADATA device that is a fabric multipath head: the operator's
/// connects (the mount's argument names the node, so the daemon owns no
/// record for it), judged per path, their policy set, their liveness
/// counted. Nothing of it is released at the teardown.
pub const MetaBound = struct {
    path: []const u8,
    subsys_buf: [32]u8 = undefined,
    subsys_len: usize = 0,
    subsysnqn_buf: [configfs.NVMF_NQN_SIZE]u8 = undefined,
    subsysnqn_len: usize = 0,
    instances: [set_config.MAX_FABRIC_PATHS]u32 = undefined,
    n_paths: usize = 0,
    iopolicy_buf: [32]u8 = undefined,
    iopolicy_len: usize = 0,

    pub fn subsys(self: *const MetaBound) []const u8 {
        return self.subsys_buf[0..self.subsys_len];
    }
    pub fn subsysnqn(self: *const MetaBound) []const u8 {
        return self.subsysnqn_buf[0..self.subsysnqn_len];
    }
    pub fn iopolicy(self: *const MetaBound) []const u8 {
        return self.iopolicy_buf[0..self.iopolicy_len];
    }
    pub fn paths(self: *const MetaBound) []const u32 {
        return self.instances[0..self.n_paths];
    }
};

/// The mount's fabric plan: the identity it connects under and what it
/// bound; `release` is the teardown's.
pub const Plan = struct {
    gpa: Allocator,
    identity: ?initiator.Identity = null,
    bound: std.ArrayList(Bound) = .empty,
    meta: std.ArrayList(MetaBound) = .empty,
    /// The explicit pair the metadata heads were judged under (borrowed
    /// from the mount's environment); null = the operator's host default.
    meta_pair: ?initiator.HostPair = null,

    pub fn init(gpa: Allocator) Plan {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *Plan) void {
        self.bound.deinit(self.gpa);
        for (self.meta.items) |m| self.gpa.free(m.path);
        self.meta.deinit(self.gpa);
    }

    /// The pair as the ladder and the disconnect filter compare it.
    pub fn pair(self: *const Plan) ?initiator.HostPair {
        const id = &(self.identity orelse return null);
        return .{ .hostnqn = id.hostnqn(), .hostid = id.hostid() };
    }

    /// The write cache's MEMBER identity (0003 §BG: the PR registrant the
    /// mount presents; 0 when the set has no fabric record — a local
    /// device's one member per host).
    pub fn memberIdentity(self: *const Plan) u64 {
        const id = &(self.identity orelse return 0);
        return staging.memberIdentity(id.hostnqn(), id.hostid());
    }

    /// Bind every data volume of `cfg` (in place: a bound volume's `uri`
    /// becomes the live node path) and judge the META paths. `mountpoint`
    /// salts the derived identity.
    pub fn bind(self: *Plan, env: *const std.process.Environ.Map, cfg: *set_config.Config, meta_paths: []const []const u8, mountpoint: []const u8, diag: *Refusal) error{Refused}!void {
        var any_record = false;
        for (cfg.data) |d| if (d.state != .retired and d.fabric != null) {
            any_record = true;
        };
        const explicit: ?initiator.HostPair = switch (pr.resolveExplicitIdentity(env, .{})) {
            .refusal => |r| {
                diag.* = .{ .identity = .{ .pair = r } };
                return error.Refused;
            },
            .pair => |p| .{ .hostnqn = p.hostnqn, .hostid = p.hostid },
            .none => null,
        };
        self.meta_pair = explicit;
        if (any_record) {
            switch (resolveMountIdentity(env, mountpoint)) {
                .identity => |id| self.identity = id,
                .refusal => |r| {
                    diag.* = .{ .identity = r };
                    return error.Refused;
                },
            }
            const id = &self.identity.?;
            log.info("mount: fabric host identity hostnqn={s} hostid={s} ({s}); {s}", .{ id.hostnqn(), id.hostid(), @tagName(id.source), initiator.readHostScoped().announce() });
        }
        const policy = multipath.iopolicyKnob(env);
        // The META plane: the operator's connects. A fabric multipath head
        // is judged PER PATH under an explicit pair (rule 2: every serving
        // controller carries it; another identity among them is the merged
        // shape), its policy set and its paths counted whatever the
        // identity posture — the mount's I/O rides it.
        for (meta_paths) |path| {
            var shape = Shape.of(self.gpa, initiator.SYSFS_ROOT, path);
            if (explicit) |want| {
                const v = initiator.fabricLadderVerdict(shape.volume(.meta, false), want);
                switch (v) {
                    .accept => {},
                    .accept_unverified => {
                        var vb: [512]u8 = undefined;
                        var vw: Io.Writer = .fixed(&vb);
                        v.write(&vw) catch {};
                        log.warn("mount: metadata volume {s}: {s}", .{ path, vw.buffered() });
                    },
                    else => {
                        stats.fabric_ladder_refusals.inc();
                        diag.* = .{ .ladder = .{ .vol_tag = 0, .path = path, .verdict = v } };
                        return error.Refused;
                    },
                }
            }
            if (shape.subsys_len != 0 and shape.n_paths != 0) try self.bindMeta(path, &shape, explicit, policy, diag);
        }
        // Nothing to connect and nothing to judge: a local set, or the
        // operator's connects under the host default.
        if (!any_record and explicit == null) return;
        // The DATA plane.
        for (cfg.data) |*d| {
            if (d.state == .retired) continue;
            if (d.fabric) |f| {
                try self.bindVolume(d, f, policy, env, diag);
                continue;
            }
            const want = explicit orelse continue;
            var shape = Shape.of(self.gpa, initiator.SYSFS_ROOT, d.uri);
            const v = initiator.fabricLadderVerdict(shape.volume(.data, false), want);
            if (v != .accept) {
                stats.fabric_ladder_refusals.inc();
                diag.* = .{ .ladder = .{ .vol_tag = d.vol_tag, .path = d.uri, .verdict = v } };
                return error.Refused;
            }
        }
    }

    /// The reuse-or-connect of every path of one record, the head's
    /// serving set judged exactly ours, the iopolicy written; the volume's
    /// `uri` becomes the live node.
    fn bindVolume(self: *Plan, d: *set_config.DataVolume, f: set_config.FabricEndpoint, policy: multipath.Iopolicy, env: *const std.process.Environ.Map, diag: *Refusal) error{Refused}!void {
        const id = &self.identity.?;
        const pair_: initiator.HostPair = .{ .hostnqn = id.hostnqn(), .hostid = id.hostid() };
        const timeouts = multipath.resolveConnectTimeouts(env);
        var b: Bound = .{ .vol_tag = d.vol_tag };
        @memcpy(b.subnqn_buf[0..f.subnqn.len], f.subnqn);
        b.subnqn_len = f.subnqn.len;
        // Several paths need the kernel's native multipath to merge them
        // into one head: refused BEFORE any connect, naming the parameter.
        if (f.paths.len > 1 and multipath.readMultipathParam() != .on) {
            diag.* = .{ .head = .{ .vol_tag = d.vol_tag, .subnqn = f.subnqn, .verdict = .no_head } };
            return error.Refused;
        }
        var dev_path: [40]u8 = undefined;
        var dev: []const u8 = "";
        var last_err: ?initiator.ConnectError = null;
        var last_spec: initiator.ConnectSpec = undefined;
        for (f.paths) |path| {
            var ab: [48]u8 = undefined;
            const spec: initiator.ConnectSpec = .{ .trtype = switch (path.trtype) {
                .tcp => .tcp,
                .loop => .loop,
            }, .traddr = path.traddrText(&ab), .trsvcid = path.trsvcid, .subnqn = f.subnqn, .hostnqn = id.hostnqn(), .hostid = id.hostid(), .timeouts = timeouts };
            var bp: BoundPath = .{ .path = path, .instance = 0, .reused = false };
            if (self.findOurs(f.subnqn, spec)) |ours| {
                bp.instance = ours.instance;
                bp.reused = true;
                dev = ours.ns.blockPath(&dev_path);
                stats.fabric_daemon_reuses.inc();
                log.warn("mount: data volume vol-{x:0>16}: reusing controller nvme{d} ({s}) already riding this mount's identity for {s} at {s}:{d} — a previous incarnation's association; it is released with this mount", .{ d.vol_tag, ours.instance, dev, f.subnqn, spec.traddr, spec.trsvcid });
            } else {
                const c = initiator.connect(self.gpa, spec, null) catch |e| {
                    stats.fabric_daemon_connect_failures.inc();
                    last_err = e;
                    last_spec = spec;
                    b.paths_failed += 1;
                    var eb: [1024]u8 = undefined;
                    var ew: Io.Writer = .fixed(&eb);
                    initiator.writeConnectRefusal(&ew, e, spec) catch {};
                    log.warn("mount: data volume vol-{x:0>16}: path {s}:{d} of {s} did not come up: {s}", .{ d.vol_tag, spec.traddr, spec.trsvcid, f.subnqn, ew.buffered() });
                    continue;
                };
                bp.instance = c.instance;
                dev = c.ns.blockPath(&dev_path);
                stats.fabric_daemon_connects.inc();
                log.info("mount: data volume vol-{x:0>16}: connected {s} at {s}:{d} as nvme{d} (cntlid {d}) → {s} (nsid {d}); fast_io_fail_tmo {d} s, ctrl_loss_tmo {d} s, reconnect_delay {d} s", .{ d.vol_tag, f.subnqn, spec.traddr, spec.trsvcid, c.instance, c.cntlid, dev, c.ns.nsid, timeouts.fast_io_fail_tmo, timeouts.ctrl_loss_tmo, timeouts.reconnect_delay });
            }
            b.paths[b.n_paths] = bp;
            b.n_paths += 1;
        }
        if (b.n_paths == 0) {
            diag.* = .{ .connect = .{ .vol_tag = d.vol_tag, .subnqn = f.subnqn, .err = last_err.?, .spec = last_spec, .paths = f.paths.len } };
            return error.Refused;
        }
        @memcpy(b.device_buf[0..dev.len], dev);
        b.device_len = dev.len;
        // The connects are owned from here: a refusal below releases them
        // with the plan.
        self.bound.append(self.gpa, b) catch {
            var ib: [set_config.MAX_FABRIC_PATHS]u32 = undefined;
            for (b.instances(&ib)) |inst| initiator.deleteController(inst) catch {};
            diag.* = .out_of_memory;
            return error.Refused;
        };
        const bound = &self.bound.items[self.bound.items.len - 1];
        if (bound.paths_failed != 0) {
            stats.fabric_paths_degraded.add(bound.paths_failed);
            log.warn("mount: data volume vol-{x:0>16}: DEGRADED — {d} of {d} path(s) of {s} did not come up; serving on {d} (`squeezefs nvmeof status` lists the paths; a path that comes back later needs a remount to be used)", .{ d.vol_tag, bound.paths_failed, f.paths.len, f.subnqn, bound.n_paths });
        }
        // The head's serving set: exactly ours, or refused. One path on a
        // kernel without a head is the per-controller node, as before.
        var ib: [set_config.MAX_FABRIC_PATHS]u32 = undefined;
        const head = multipath.headVerdict(self.gpa, initiator.SYSFS_ROOT, bound.instances(&ib), pair_, &bound.subsys_buf) catch {
            diag.* = .out_of_memory;
            return error.Refused;
        };
        bound.subsys_len = head.subsys_len;
        switch (head.verdict) {
            .exactly_ours => {},
            .no_head => if (f.paths.len > 1) {
                diag.* = .{ .head = .{ .vol_tag = d.vol_tag, .subnqn = f.subnqn, .verdict = head.verdict } };
                return error.Refused;
            },
            .foreign, .missing => {
                stats.fabric_ladder_refusals.inc();
                diag.* = .{ .head = .{ .vol_tag = d.vol_tag, .subnqn = f.subnqn, .verdict = head.verdict } };
                return error.Refused;
            },
        }
        applyIopolicy(bound, policy);
        stats.fabric_paths_live.set(self.liveCount());
        // The grain the planes will DMA at (O_DIRECT on every data device).
        const probe = align_probe.probeGrain(dev);
        _ = align_probe.admitMetaIoGrain(probe.grain) catch {
            diag.* = .{ .grain = .{ .vol_tag = d.vol_tag, .path = bound.device(), .grain = probe.grain } };
            return error.Refused;
        };
        log.info("mount: data volume vol-{x:0>16}: {s} DMA grain {d} ({s}); {d} path(s) under {s}", .{ d.vol_tag, dev, probe.grain, @tagName(probe.source), bound.n_paths, if (bound.subsys_len == 0) "no multipath head" else bound.subsys() });
        const live = self.gpa.dupe(u8, dev) catch {
            diag.* = .out_of_memory;
            return error.Refused;
        };
        self.gpa.free(d.uri);
        d.uri = live;
    }

    /// `SQUEEZEFS_FABRIC_IOPOLICY` written to the bound subsystem and read
    /// back; a kernel keeping its own policy is loud, never a refusal.
    fn applyIopolicy(b: *Bound, policy: multipath.Iopolicy) void {
        if (b.subsys_len == 0) return;
        var got: [32]u8 = undefined;
        const outcome = multipath.setIopolicy(initiator.SYSFS_ROOT, b.subsys(), policy, &got);
        var ob: [256]u8 = undefined;
        var ow: Io.Writer = .fixed(&ob);
        outcome.write(&ow, policy) catch {};
        switch (outcome) {
            .set => {
                stats.fabric_iopolicy_sets.inc();
                initiator.fill(&b.iopolicy_buf, &b.iopolicy_len, policy.name());
                log.info("mount: data volume vol-{x:0>16}: {s}: {s}", .{ b.vol_tag, b.subsys(), ow.buffered() });
            },
            .mismatch => |kept| {
                stats.fabric_iopolicy_failures.inc();
                initiator.fill(&b.iopolicy_buf, &b.iopolicy_len, kept);
                log.warn("mount: data volume vol-{x:0>16}: {s}: {s}", .{ b.vol_tag, b.subsys(), ow.buffered() });
            },
            .write_refused, .absent => {
                stats.fabric_iopolicy_failures.inc();
                var ib: [32]u8 = undefined;
                initiator.fill(&b.iopolicy_buf, &b.iopolicy_len, multipath.readIopolicy(initiator.SYSFS_ROOT, b.subsys(), &ib) orelse "");
                log.warn("mount: data volume vol-{x:0>16}: {s}: {s}", .{ b.vol_tag, b.subsys(), ow.buffered() });
            },
        }
    }

    /// A metadata device served by a fabric head: recorded with its paths,
    /// its policy written (the knob's, read back), its liveness counted.
    fn bindMeta(self: *Plan, path: []const u8, shape: *const Shape, explicit: ?initiator.HostPair, policy: multipath.Iopolicy, diag: *Refusal) error{Refused}!void {
        var m: MetaBound = .{ .path = self.gpa.dupe(u8, path) catch {
            diag.* = .out_of_memory;
            return error.Refused;
        } };
        errdefer self.gpa.free(m.path);
        @memcpy(m.subsys_buf[0..shape.subsys_len], shape.subsys_buf[0..shape.subsys_len]);
        m.subsys_len = shape.subsys_len;
        if (shape.ctrl) |*c| initiator.fill(&m.subsysnqn_buf, &m.subsysnqn_len, c.subsysnqnOf());
        m.n_paths = @min(shape.n_paths, m.instances.len);
        @memcpy(m.instances[0..m.n_paths], shape.instances[0..m.n_paths]);
        var got: [32]u8 = undefined;
        const outcome = multipath.setIopolicy(initiator.SYSFS_ROOT, m.subsys(), policy, &got);
        var ob: [256]u8 = undefined;
        var ow: Io.Writer = .fixed(&ob);
        outcome.write(&ow, policy) catch {};
        switch (outcome) {
            .set => {
                stats.fabric_iopolicy_sets.inc();
                initiator.fill(&m.iopolicy_buf, &m.iopolicy_len, policy.name());
                log.info("mount: metadata volume {s}: {s} ({d} path(s){s}): {s}", .{ path, m.subsys(), m.n_paths, if (explicit != null) " under this mount's identity pair" else "", ow.buffered() });
            },
            .mismatch => |kept| {
                stats.fabric_iopolicy_failures.inc();
                initiator.fill(&m.iopolicy_buf, &m.iopolicy_len, kept);
                log.warn("mount: metadata volume {s}: {s}: {s}", .{ path, m.subsys(), ow.buffered() });
            },
            .write_refused, .absent => {
                stats.fabric_iopolicy_failures.inc();
                var ib: [32]u8 = undefined;
                initiator.fill(&m.iopolicy_buf, &m.iopolicy_len, multipath.readIopolicy(initiator.SYSFS_ROOT, m.subsys(), &ib) orelse "");
                log.warn("mount: metadata volume {s}: {s}: {s}", .{ path, m.subsys(), ow.buffered() });
            },
        }
        self.meta.append(self.gpa, m) catch {
            diag.* = .out_of_memory;
            return error.Refused;
        };
        stats.fabric_paths_live.set(self.liveCount());
    }

    /// The live serving controllers summed over the owned subsystems — the
    /// data heads' under our pair, the metadata heads' under the explicit
    /// pair when there is one (every path, as the ladder accepted them)
    /// else any — the `fabric_paths_live` gauge; sampled at bind and at
    /// every render, allocation-free.
    fn liveCount(self: *const Plan) u64 {
        var n: u64 = 0;
        if (self.pair()) |p| for (self.bound.items) |*b| {
            n += if (b.subsys_len != 0) multipath.countLiveServing(initiator.SYSFS_ROOT, b.subsys(), p) else multipath.countOursLive(initiator.SYSFS_ROOT, b.subnqn(), p);
        };
        for (self.meta.items) |*m| n += multipath.countLiveServing(initiator.SYSFS_ROOT, m.subsys(), self.meta_pair);
        return n;
    }

    /// A live controller of `subnqn` carrying our pair ON THE PATH's
    /// address (`traddr=`/`trsvcid=`; a loop path has none) with a
    /// namespace node under it (the reuse arm).
    fn findOurs(self: *Plan, subnqn: []const u8, spec: initiator.ConnectSpec) ?initiator.Connected {
        const p = self.pair() orelse return null;
        const ctrls = initiator.listControllers(self.gpa, initiator.SYSFS_ROOT) catch return null;
        defer self.gpa.free(ctrls);
        for (ctrls) |c| {
            if (!std.mem.eql(u8, c.subsysnqnOf(), subnqn) or !c.hasIdentity(p.hostnqn, p.hostid)) continue;
            if (!c.isLive()) continue;
            if (!std.mem.eql(u8, c.transportOf(), spec.trtype.name())) continue;
            if (spec.trtype == .tcp) {
                const a = multipath.addressParts(c.addressOf());
                var sb: [8]u8 = undefined;
                const svc = std.fmt.bufPrint(&sb, "{d}", .{spec.trsvcid}) catch unreachable;
                if (!std.mem.eql(u8, a.traddr, spec.traddr) or !std.mem.eql(u8, a.trsvcid, svc)) continue;
            }
            const ns = (initiator.controllerNamespace(self.gpa, initiator.SYSFS_ROOT, c.instance) catch null) orelse continue;
            if (!ns.char_present) continue;
            return .{ .instance = c.instance, .cntlid = 0, .ns = ns };
        }
        return null;
    }

    /// The teardown: every controller of a bound subsystem under our pair
    /// (`--identity-only`, every path), each awaited gone. After the
    /// device fds closed.
    pub fn release(self: *Plan) void {
        const p = self.pair() orelse return;
        for (self.bound.items) |*b| {
            const n = initiator.disconnect(self.gpa, .{ .subsysnqn = b.subnqn(), .identity = .{ .hostnqn = p.hostnqn, .hostid = p.hostid } }) catch |e| {
                log.err("umount: data volume vol-{x:0>16}: the disconnect of {s} failed ({s}) — `squeezefs nvmeof disconnect --identity-only {s}`", .{ b.vol_tag, b.subnqn(), @errorName(e), b.subnqn() });
                continue;
            };
            stats.fabric_daemon_disconnects.add(n);
            for (b.pathsUp()) |bp| _ = initiator.awaitControllerGone(bp.instance);
            log.info("umount: data volume vol-{x:0>16}: {d} controller(s) of {s} under this mount's identity released", .{ b.vol_tag, n, b.subnqn() });
        }
        self.bound.clearRetainingCapacity();
        stats.fabric_paths_live.set(0);
    }

    /// The `.stats` rows: the bound volumes, each with its paths; the
    /// metadata heads with theirs (each path's controller read live); the
    /// live-path gauge re-sampled off sysfs (allocation-free).
    pub fn writeStatsJson(self: *const Plan, w: *Io.Writer) Io.Writer.Error!void {
        if (self.bound.items.len != 0 or self.meta.items.len != 0) stats.fabric_paths_live.set(self.liveCount());
        try w.writeAll("\"fabric_meta_bound\":[");
        for (self.meta.items, 0..) |*m, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"path\":\"{s}\",\"subsystem\":\"{s}\",\"subsysnqn\":\"{s}\",\"iopolicy\":\"{s}\",\"paths\":[", .{ m.path, m.subsys(), m.subsysnqn(), m.iopolicy() });
            for (m.paths(), 0..) |inst, j| {
                if (j != 0) try w.writeByte(',');
                var nb: [16]u8 = undefined;
                const name = std.fmt.bufPrint(&nb, "nvme{d}", .{inst}) catch unreachable;
                if (initiator.readController(initiator.SYSFS_ROOT, name)) |c| {
                    const a = multipath.addressParts(c.addressOf());
                    try w.print("{{\"instance\":{d},\"transport\":\"{s}\",\"traddr\":\"{s}\",\"trsvcid\":\"{s}\",\"state\":\"{s}\"}}", .{ inst, c.transportOf(), a.traddr, a.trsvcid, c.stateOf() });
                } else try w.print("{{\"instance\":{d},\"state\":\"gone\"}}", .{inst});
            }
            try w.writeAll("]}");
        }
        try w.writeAll("],\"fabric_daemon_bound\":[");
        for (self.bound.items, 0..) |*b, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"vol_tag\":\"vol-{x:0>16}\",\"subnqn\":\"{s}\",\"device\":\"{s}\",\"subsystem\":\"{s}\",\"iopolicy\":\"{s}\",\"paths_failed\":{d},\"paths\":[", .{ b.vol_tag, b.subnqn(), b.device(), b.subsys(), b.iopolicy(), b.paths_failed });
            for (b.pathsUp(), 0..) |bp, j| {
                if (j != 0) try w.writeByte(',');
                var ab: [48]u8 = undefined;
                try w.print("{{\"instance\":{d},\"trtype\":\"{s}\",\"traddr\":\"{s}\",\"trsvcid\":{d},\"reused\":{s}}}", .{ bp.instance, @tagName(bp.path.trtype), bp.path.traddrText(&ab), bp.path.trsvcid, if (bp.reused) "true" else "false" });
            }
            try w.writeAll("]}");
        }
        try w.writeByte(']');
    }
};

/// The data-plane connect identity: the explicit pair → `/etc/nvme` both
/// → the machine-id derivation salted by the mountpoint (stable per mount
/// slot: a remount at the same mountpoint presents the same pair; two
/// mountpoints on one host never share a controller).
pub fn resolveMountIdentity(env: *const std.process.Environ.Map, mountpoint: []const u8) initiator.Resolved {
    const etc = proc.EtcNvme.read();
    var mb: [128]u8 = undefined;
    const mid = proc.readSmall(initiator.MACHINE_ID_PATH, &mb);
    const r = resolveMountIdentityWith(env, &etc, mid, mountpoint);
    if (r == .identity) stats.fabric_host_identity_source.set(@backingInt(r.identity.source));
    return r;
}

/// Pure over the sources (the fixture tests).
pub fn resolveMountIdentityWith(env: *const std.process.Environ.Map, etc: *const proc.EtcNvme, machine_id: ?[]const u8, mountpoint: []const u8) initiator.Resolved {
    const mid = std.mem.trim(u8, machine_id orelse "", " \t\r\n");
    var salted: [128 + linux.PATH_MAX]u8 = undefined;
    const salted_id: ?[]const u8 = if (mid.len == 0) null else std.fmt.bufPrint(&salted, "{s}\x00{s}", .{ mid, mountpoint }) catch null;
    return initiator.resolveIdentity(env, .{}, etc, salted_id);
}

/// The ladder's shape for a device path: on a multipath kernel the node
/// is a HEAD and its serving controllers are the paths — the first stands
/// for the device (`actual`), the merged shape is more than one identity
/// among them (`multipath.servingShape`); on a kernel without a head, the
/// per-controller node's one controller and the subsystem-wide merged
/// detector. `ctrl` null = a local device / a regular file. The
/// controller's attrs live in this value, so the verdict's slices borrow
/// from it.
pub const Shape = struct {
    ctrl: ?initiator.Controller,
    merged: bool,
    /// The head's `nvme-subsys<K>` (empty: no head).
    subsys_buf: [32]u8 = undefined,
    subsys_len: usize = 0,
    /// The head's fabric serving controllers (0: no head, or local).
    instances: [set_config.MAX_FABRIC_PATHS]u32 = undefined,
    n_paths: usize = 0,

    pub fn of(gpa: Allocator, root: []const u8, path: []const u8) Shape {
        var s: Shape = .{ .ctrl = null, .merged = false };
        const base = std.fs.path.basename(path);
        if (initiator.splitNamespaceName(base) == null) return s;
        const sub = (multipath.subsystemOfNode(gpa, root, base, &s.subsys_buf) catch null) orelse {
            const c = controllerOfNode(root, path) orelse return s;
            s.ctrl = c;
            s.merged = initiator.multipathMergedShape(gpa, root, c.subsysnqnOf()) catch false;
            return s;
        };
        s.subsys_len = sub.len;
        const serving = multipath.servingControllers(gpa, root, sub) catch return s;
        defer gpa.free(serving);
        const shape = multipath.servingShape(serving);
        if (shape.local) {
            s.subsys_len = 0;
            return s;
        }
        s.merged = shape.merged;
        if (shape.first) |i| s.ctrl = serving[i];
        for (serving) |*c| {
            if (s.n_paths == s.instances.len) break;
            s.instances[s.n_paths] = c.instance;
            s.n_paths += 1;
        }
        // Numeric order (the dir lists names: nvme10 before nvme9).
        std.mem.sort(u32, s.instances[0..s.n_paths], {}, std.sort.asc(u32));
        return s;
    }

    pub fn volume(self: *Shape, plane: initiator.Plane, has_record: bool) initiator.VolumeShape {
        return .{ .plane = plane, .has_record = has_record, .actual = if (self.ctrl) |*c| .{ .hostnqn = c.hostnqnOf(), .hostid = c.hostidOf() } else null, .merged_shape = self.merged };
    }
};

/// `/dev/nvme<N>n<Y>` → the fabric controller `nvme<N>` (the per-
/// controller node of a non-multipath kernel); honest null for anything
/// else (a file, a PCIe namespace reads as a non-fabric controller → null).
pub fn controllerOfNode(root: []const u8, path: []const u8) ?initiator.Controller {
    const base = std.fs.path.basename(path);
    const parts = initiator.splitNamespaceName(base) orelse return null;
    var nb: [16]u8 = undefined;
    const name = std.fmt.bufPrint(&nb, "nvme{d}", .{parts.ctrl}) catch return null;
    const c = initiator.readController(root, name) orelse return null;
    if (!c.isFabric()) return null;
    return c;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const testkit = @import("../../core/testkit.zig");

test "the mount identity: the explicit pair wins; the derived default is salted by the mountpoint (two mountpoints differ, a remount at one agrees, the unsalted verb identity differs from both); no source refuses" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var etc: proc.EtcNvme = .{};
    const a1 = resolveMountIdentityWith(&env, &etc, "0123456789abcdef0123456789abcdef\n", "/mnt/a");
    const a2 = resolveMountIdentityWith(&env, &etc, "0123456789abcdef0123456789abcdef", "/mnt/a");
    const b = resolveMountIdentityWith(&env, &etc, "0123456789abcdef0123456789abcdef", "/mnt/b");
    try testing.expect(a1 == .identity and a2 == .identity and b == .identity);
    try testing.expectEqual(initiator.IdentitySource.machine_id, a1.identity.source);
    try testing.expectEqualStrings(a1.identity.hostid(), a2.identity.hostid());
    try testing.expect(!std.mem.eql(u8, a1.identity.hostid(), b.identity.hostid()));
    const verb = initiator.resolveIdentity(&env, .{}, &etc, "0123456789abcdef0123456789abcdef");
    try testing.expect(!std.mem.eql(u8, verb.identity.hostid(), a1.identity.hostid()));
    try testing.expect(std.mem.startsWith(u8, a1.identity.hostnqn(), initiator.HOSTNQN_UUID_PREFIX));
    try testing.expect(resolveMountIdentityWith(&env, &etc, null, "/mnt/a").refusal == .no_source);
    try env.put("SQUEEZEFS_HOSTNQN", "nqn.2014-08.org.nvmexpress:uuid:2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c");
    try env.put("SQUEEZEFS_HOSTID", "2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c");
    const e = resolveMountIdentityWith(&env, &etc, null, "/mnt/a");
    try testing.expectEqual(initiator.IdentitySource.explicit, e.identity.source);
    try testing.expectEqualStrings("2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c", e.identity.hostid());
    // The write cache's member scope follows the identity (0003 §BG): a
    // plan with none is the local device's one member (0); two mount
    // points' derived identities are two members; the explicit pair is
    // one member wherever it mounts.
    var none = Plan.init(gpa);
    defer none.deinit();
    try testing.expectEqual(@as(u64, 0), none.memberIdentity());
    var pa = Plan.init(gpa);
    defer pa.deinit();
    pa.identity = a1.identity;
    var pb = Plan.init(gpa);
    defer pb.deinit();
    pb.identity = b.identity;
    try testing.expect(pa.memberIdentity() != 0 and pa.memberIdentity() != pb.memberIdentity());
    pb.identity = a2.identity;
    try testing.expectEqual(pa.memberIdentity(), pb.memberIdentity());
    pa.identity = e.identity;
    try testing.expectEqual(staging.memberIdentity(e.identity.hostnqn(), e.identity.hostid()), pa.memberIdentity());
}

test "a plan over a config with no records and no explicit pair binds nothing; a half pair refuses; a non-NVMe path has no controller" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const vols = [_]set_config.DataVolume{.{ .vol_tag = 0x1111_2222_3333_4444, .state = .active, .device_uuid = @splat(1), .added_ts = 1_700_000_000, .capacity_blocks = 254, .volume_len = 1 << 30, .uri = "/build/tmp/mount-fabric-d0.img" }};
    var cfg = try set_config.birth(gpa, @splat(7), .{ .compression = .none, .encryption = .none, .block_size = 4 << 20 }, null, &vols);
    defer cfg.deinit(gpa);
    var plan = Plan.init(gpa);
    defer plan.deinit();
    var diag: Refusal = undefined;
    try plan.bind(&env, &cfg, &.{"/build/tmp/not-a-device"}, "/mnt/x", &diag);
    try testing.expect(plan.identity == null and plan.bound.items.len == 0);
    try env.put("SQUEEZEFS_HOSTNQN", "nqn.2014-08.org.nvmexpress:uuid:2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c");
    try testing.expectError(error.Refused, plan.bind(&env, &cfg, &.{}, "/mnt/x", &diag));
    try testing.expect(diag == .identity);
    try testing.expect(controllerOfNode(initiator.SYSFS_ROOT, "/build/tmp/some.img") == null);
    try testing.expect(controllerOfNode(initiator.SYSFS_ROOT, "/dev/nvme99999n1") == null);
    var buf: [256]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try plan.writeStatsJson(&w);
    try testing.expectEqualStrings("\"fabric_meta_bound\":[],\"fabric_daemon_bound\":[]", w.buffered());
}

test "the metadata device's shape over a multipath fixture: a head served by two paths of one identity reads as two paths under one subsystem with that identity standing for the device (rule 2 per path accepts); a third path under another identity is the merged shape (refused); a local PCIe head is no fabric shape; a per-controller node without a head is the one controller; a file is nothing" {
    const gpa = testing.allocator;
    var fx = initiator.Fixture.init(gpa, testing.io, "metashape") catch return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer fx.deinit();
    const pair_a: initiator.HostPair = .{ .hostnqn = "nqn.hostA", .hostid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" };
    const pair_b: initiator.HostPair = .{ .hostnqn = "nqn.hostB", .hostid = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb" };
    try fx.controller(1, "tcp", "live", "nqn.meta", pair_a.hostnqn, pair_a.hostid, "traddr=127.0.0.1,trsvcid=4498");
    try fx.controller(2, "tcp", "live", "nqn.meta", pair_a.hostnqn, pair_a.hostid, "traddr=127.0.0.2,trsvcid=4498");
    try fx.file("class/nvme-subsystem/nvme-subsys1/subsysnqn", .{}, "nqn.meta\n");
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme1", .{});
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme2", .{});
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme1n1", .{});
    var two = Shape.of(gpa, fx.root, "/dev/nvme1n1");
    try testing.expectEqualStrings("nvme-subsys1", two.subsys_buf[0..two.subsys_len]);
    try testing.expectEqual(@as(usize, 2), two.n_paths);
    try testing.expectEqualSlices(u32, &.{ 1, 2 }, two.instances[0..2]);
    try testing.expect(!two.merged);
    try testing.expectEqualStrings("nqn.meta", two.ctrl.?.subsysnqnOf());
    try testing.expectEqual(initiator.Verdict.accept, initiator.fabricLadderVerdict(two.volume(.meta, false), pair_a));
    try testing.expectEqual(initiator.Verdict.refuse_identity, initiator.fabricLadderVerdict(two.volume(.meta, false), pair_b));
    // A third path under identity B: merged under one head.
    try fx.controller(3, "tcp", "live", "nqn.meta", pair_b.hostnqn, pair_b.hostid, "traddr=127.0.0.3,trsvcid=4498");
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme3", .{});
    var mixed = Shape.of(gpa, fx.root, "/dev/nvme1n1");
    try testing.expect(mixed.merged);
    try testing.expectEqual(@as(usize, 3), mixed.n_paths);
    try testing.expectEqual(initiator.Verdict.refuse_merged, initiator.fabricLadderVerdict(mixed.volume(.meta, false), pair_a));
    // The host's PCIe disk with a head of its own: local, no fabric shape.
    try fx.controller(0, "pcie", "live", "nqn.local", "", "", "0000:c2:00.0");
    try fx.file("class/nvme-subsystem/nvme-subsys0/subsysnqn", .{}, "nqn.local\n");
    try fx.dir("class/nvme-subsystem/nvme-subsys0/nvme0", .{});
    try fx.dir("class/nvme-subsystem/nvme-subsys0/nvme0n1", .{});
    var local = Shape.of(gpa, fx.root, "/dev/nvme0n1");
    try testing.expect(local.ctrl == null and local.n_paths == 0 and local.subsys_len == 0);
    try testing.expectEqual(initiator.Verdict.accept_unverified, initiator.fabricLadderVerdict(local.volume(.meta, false), pair_a));
    // No head (a kernel without native multipath): the per-controller node.
    try fx.controller(7, "loop", "live", "nqn.plain", pair_a.hostnqn, pair_a.hostid, "");
    try fx.namespace(7, 1, "77777777-2222-3333-4444-555555555555\n", true);
    var plain = Shape.of(gpa, fx.root, "/dev/nvme7n1");
    try testing.expectEqual(@as(u32, 7), plain.ctrl.?.instance);
    try testing.expect(plain.subsys_len == 0 and plain.n_paths == 0 and !plain.merged);
    try testing.expectEqual(initiator.Verdict.accept, initiator.fabricLadderVerdict(plain.volume(.meta, false), pair_a));
    const file = Shape.of(gpa, fx.root, "/build/tmp/meta.img");
    try testing.expect(file.ctrl == null and file.n_paths == 0);
}
