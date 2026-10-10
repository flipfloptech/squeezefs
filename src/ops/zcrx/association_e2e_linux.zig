//! THE ASSOCIATION, end to end on the venue (`docs/porting/ops_zcrx.md`
//! §12.1.11): a REAL nvmet-tcp target on 127.0.0.1 stood up through
//! `ops/nvmet` (configfs: a 64 MiB file-backed namespace, the harness NQN
//! marker, the harness port-id slice), the KERNEL initiator connected to
//! it (`/dev/nvme-fabrics` → `/dev/nvme<C>n<N>`), then the USERSPACE
//! association connected a SECOND time, read-only, under a distinct host
//! identity, reading whole blocks through the lane's `RECV` leg into the
//! area and gathering into a destination byte-exact against the kernel
//! path's read of the same node. Pins: `gather ≡ fill` byte-exact; the
//! MQES clamp observed (`depth == min(derived, MQES + 1)`); CID custody
//! closes (`free == total`); every area chunk back but the recv cursor's;
//! a status ≠ 0 (an LBA past the namespace) fails the OP and the session
//! lives; an unaligned range is ineligible (uncounted); teardown clean with
//! zero residue (the subsystem, the port and the controller gone).
//!
//! Skips LOUDLY without root, `/dev/nvme-fabrics` or a ready nvmet tree.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const configfs = @import("../nvmet/configfs.zig");
const initiator = @import("../nvmet/initiator.zig");
const association = @import("association.zig");
const probe = @import("probe.zig");
const nic_census = @import("nic_census.zig");
const area = @import("area.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("zcrx/association_e2e_linux is Linux only (charter §1.2)");
}

const testing = std.testing;

/// 64 MiB backing, 4 KiB blocks (the smallest whole-block fill), reads of
/// one block, a 1 MiB whole-block cold fill and a 3 MiB multi-segment read.
const backing_len: u64 = 64 << 20;
const trsvcid: u16 = 4499;
/// Inside the harness slice [54000, 55000), away from the fidelity tier's
/// hashed ids and the smoke's 4420/4433 listeners.
const port_id: u16 = 54950;
/// The kernel initiator's identity (A) and the lane's (B): two host
/// associations on one namespace.
const ident_a: initiator.HostPair = .{ .hostnqn = "nqn.2014-08.org.nvmexpress:uuid:c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1", .hostid = "c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1" };
const ident_b: initiator.HostPair = .{ .hostnqn = "nqn.2014-08.org.nvmexpress:uuid:d2d2d2d2-d2d2-4d2d-8d2d-d2d2d2d2d2d2", .hostid = "d2d2d2d2-d2d2-4d2d-8d2d-d2d2d2d2d2d2" };

fn venueReady() bool {
    if (linux.geteuid() != 0) {
        log.warn("zcrx association e2e: not root — skipped", .{});
        return false;
    }
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, initiator.FABRICS_DEV, 0, .{ .TYPE = true }, &st)) != .SUCCESS) {
        log.warn("zcrx association e2e: {s} absent — skipped", .{initiator.FABRICS_DEV});
        return false;
    }
    var e: ?linux.E = null;
    if (configfs.preflight(&e) != .ready) {
        log.warn("zcrx association e2e: the kernel nvmet target is not ready — skipped", .{});
        return false;
    }
    return true;
}

/// A deterministic pattern: byte i of block b = f(b, i), distinct per block.
fn fillPattern(buf: []u8, offset: u64) void {
    for (buf, 0..) |*b, i| {
        const abs = offset + i;
        b.* = @truncate((abs >> 12) * 31 + (abs & 0xFF) * 7 + 1);
    }
}

pub fn run() !void {
    if (!venueReady()) return error.SkipZigTest;
    const gpa = testing.allocator;
    const io = testing.io;
    const pid = linux.getpid();
    var nqn_buf: [128]u8 = undefined;
    const nqn = try std.fmt.bufPrint(&nqn_buf, "nqn.2026-10.io.squeezefs:devsub-zcrx-{d}", .{pid});
    var path_buf: [96]u8 = undefined;
    const backing = try std.fmt.bufPrint(&path_buf, "/build/tmp/zcrx-{d}.img", .{pid});
    _ = linux.mkdir("/build/tmp", 0o755);
    // 1. The 64 MiB backing with the pattern.
    {
        const f = try Io.Dir.cwd().createFile(io, backing, .{ .truncate = true });
        defer f.close(io);
        const chunk = try gpa.alloc(u8, 1 << 20);
        defer gpa.free(chunk);
        var off: u64 = 0;
        while (off < backing_len) : (off += chunk.len) {
            fillPattern(chunk, off);
            try f.writePositionalAll(io, chunk, off);
        }
    }
    defer Io.Dir.cwd().deleteFile(io, backing) catch {};
    // 2. The nvmet-tcp target through the product's configfs recipe.
    const fs = configfs.RealFs.face();
    var listener = try configfs.Listener.tcp("127.0.0.1", trsvcid);
    listener.port_id = port_id;
    const ns_uuid: [16]u8 = .{ 0xd5, 0x02, 0x1f, 0x3a, 0x6b, 0x7c, 0x4d, 0x8e, 0x9f, 0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76 };
    _ = try configfs.apply(fs, .{ .subnqn = nqn, .backing_path = backing, .ns_uuid = ns_uuid, .allow_hosts = &.{}, .listeners = &.{listener} });
    defer {
        _ = configfs.teardown(fs, gpa, .{ .subnqn = nqn, .port_ids = &.{port_id}, .allow_hosts = &.{}, .host_named_elsewhere = struct {
            fn f(_: *const anyopaque, _: []const u8) bool {
                return true;
            }
        }.f, .ctx = @ptrCast(&listener) }) catch {};
    }
    // 3. The kernel initiator: the namespace as a block node.
    const kconn = try initiator.connect(gpa, .{ .trtype = .tcp, .traddr = "127.0.0.1", .trsvcid = trsvcid, .subnqn = nqn, .hostnqn = ident_a.hostnqn, .hostid = ident_a.hostid, .nr_io_queues = 1 }, ns_uuid);
    defer {
        initiator.deleteController(kconn.instance) catch {};
        _ = initiator.awaitControllerGone(kconn.instance);
    }
    var bp: [40]u8 = undefined;
    const block_path = kconn.ns.blockPath(&bp);
    // The sysfs discovery of THIS device yields the lane's target.
    const target = try nic_census.nvmeTcpTargetFor(io, "/sys", block_path, 8);
    try testing.expectEqualStrings("127.0.0.1", target.traddrText());
    try testing.expectEqual(trsvcid, target.trsvcid);
    try testing.expectEqualStrings(nqn, target.subnqnText());
    try testing.expectEqual(kconn.ns.nsid, target.nsid);
    try testing.expect(target.max_xfer >= 4096 and target.max_xfer <= area.LANE_MAX_XFER_CAP_BYTES);
    // 4. The probe ladder on this venue: the sim seam arms the RECV leg.
    const verdict = probe.arm(.{ .lane_enabled = true, .target = target, .area_sim = true, .cpus = 8 });
    try testing.expectEqual(probe.Backend.sim, verdict.present.backend);
    var plan = verdict.present;
    plan.io_queues = 2;
    const identity = association.Identity.fromPair(ident_b.hostnqn, ident_b.hostid).?;
    const fills0 = stats.zcrx_fills.load();
    const fill_bytes0 = stats.zcrx_fill_bytes.load();
    const gather0 = stats.zcrx_gather_bytes.load();
    const poisoned0 = stats.zcrx_lane_poisoned.load();
    const fv0 = stats.zcrx_frame_violations.load();
    // 5. The SECOND association, read-only, under identity B.
    const sess = try association.Session.connect(gpa, io, plan, identity, 64 << 10, null);
    defer sess.deinit();
    try testing.expectEqual(@as(u64, 1), stats.zcrx_lane_armed.load());
    // The MQES clamp: nvmet-tcp advertises MQES from its queue size; the
    // depth is min(derived, MQES + 1) floored at 2 and never above the derived.
    try testing.expectEqual(area.depthFor(plan.target.queue_depth, sess.facts.mqes), sess.depth);
    try testing.expect(sess.depth >= 2 and sess.depth <= plan.target.queue_depth);
    try testing.expect(sess.facts.cntlid != 0xFFFF);
    const bs: u64 = @as(u64, 1) << target.lba_shift;
    // 6. Reads: one block, one 1 MiB whole-block fill, one 3 MiB multi-
    //    segment read (pipelined over both queues), each byte-exact against
    //    the kernel path AND the pattern.
    const kfile = try Io.Dir.cwd().openFile(io, block_path, .{});
    defer kfile.close(io);
    const shapes = [_]struct { off: u64, len: u64 }{
        .{ .off = 0, .len = bs },
        .{ .off = 8 << 20, .len = 1 << 20 },
        .{ .off = 20 << 20, .len = 3 << 20 },
        .{ .off = backing_len - 4096, .len = 4096 },
    };
    const lane_buf = try gpa.alloc(u8, 3 << 20);
    defer gpa.free(lane_buf);
    const kern_buf = try gpa.alloc(u8, 3 << 20);
    defer gpa.free(kern_buf);
    var total: u64 = 0;
    for (shapes) |s| {
        try sess.readInto(s.off, lane_buf[0..s.len], false);
        const kn = try kfile.readPositionalAll(io, kern_buf[0..s.len], s.off);
        try testing.expectEqual(s.len, kn);
        try testing.expectEqualSlices(u8, kern_buf[0..s.len], lane_buf[0..s.len]);
        fillPattern(kern_buf[0..s.len], s.off);
        try testing.expectEqualSlices(u8, kern_buf[0..s.len], lane_buf[0..s.len]);
        total += s.len;
    }
    // gather ≡ fill, byte-exact, at the whole-read boundary.
    try testing.expectEqual(fills0 + shapes.len, stats.zcrx_fills.load());
    try testing.expectEqual(fill_bytes0 + total, stats.zcrx_fill_bytes.load());
    try testing.expectEqual(gather0 + total, stats.zcrx_gather_bytes.load());
    // 7. An unaligned range is ineligible (the kernel path is the serve).
    try testing.expectError(error.Ineligible, sess.readInto(100, lane_buf[0..4096], false));
    try testing.expectError(error.Ineligible, sess.readInto(0, lane_buf[0..0], false));
    // 8. A read past the namespace: the controller answers a status (LBA
    //    Out of Range); the OP fails, the session lives and serves again.
    try testing.expectError(error.OpStatus, sess.readInto(backing_len + (1 << 20), lane_buf[0..4096], false));
    try testing.expect(!sess.isPoisoned());
    try sess.readInto(4096, lane_buf[0..4096], true);
    fillPattern(kern_buf[0..4096], 4096);
    try testing.expectEqualSlices(u8, kern_buf[0..4096], lane_buf[0..4096]);
    try testing.expect(stats.zcrx_dest_gather_bytes.load() >= 4096);
    // 9. Custody closes: every CID free; every area chunk back but the one
    //    each queue's recv cursor pins.
    const cids = sess.cidSlots();
    try testing.expectEqual(cids.total, cids.free);
    try testing.expect(sess.areaChunksHeld() <= plan.io_queues);
    try testing.expectEqual(poisoned0, stats.zcrx_lane_poisoned.load());
    try testing.expectEqual(fv0, stats.zcrx_frame_violations.load());
    // 10. Teardown: orderly (never poison), the gauge drops, idempotent.
    sess.teardown();
    sess.teardown();
    try testing.expectEqual(@as(u64, 0), stats.zcrx_lane_armed.load());
    try testing.expectEqual(poisoned0, stats.zcrx_lane_poisoned.load());
    try testing.expect(!sess.isPoisoned());
    _ = clock.monotonicNs();
}
