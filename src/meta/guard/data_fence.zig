const std = @import("std");
const pr = @import("pr.zig");
const nvme = @import("nvme_cmd.zig");
const proc = @import("proc.zig");
const fencing = @import("../sym/membership/fencing.zig");
const appender_page = @import("../sym/appender_page.zig");

const testing = std.testing;

fn noPages(_: *anyopaque, gpa: std.mem.Allocator) error{ Io, Corrupt, OutOfMemory }![]appender_page.Page {
    return gpa.alloc(appender_page.Page, 0);
}

test "the data admission registers this host on every DATA namespace before its first data write (decision 0003 §DJ): before it a reserved namespace refuses the host's write, after it admits it; the first registrant reserves WEAR, a later one only registers; a host already registered adopts with no device mutation; a host the metadata namespace no longer registers is refused, its data registration absent; the manager's admission preempts every data registrant the metadata namespace no longer registers (a superseded manager's host)" {
    const gpa = testing.allocator;
    const fake = @import("fake_nvme.zig");
    const m_id: [16]u8 = "manager-host-id1".*;
    const j_id: [16]u8 = "joiner-host-id-2".*;
    const o_id: [16]u8 = "old-manager-id-3".*;
    const f_id: [16]u8 = "fenced-host-id-4".*;
    // The metadata namespace: M holds WERO (it took over from O, whose key
    // the takeover removed), J registered.
    var mns = fake.Namespace.init(gpa);
    defer mns.deinit();
    var mh: fake.Host = .{ .ns = &mns, .host_id = m_id };
    var meta_m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer meta_m.deinit();
    try fencing.acquireManagerHold(&meta_m);
    var jh: fake.Host = .{ .ns = &mns, .host_id = j_id };
    var meta_j = try pr.Client.init(gpa, jh.face(), 1, 0x2222);
    defer meta_j.deinit();
    _ = try fencing.joinAsRegistrant(&meta_j, null);
    var fh: fake.Host = .{ .ns = &mns, .host_id = f_id };
    var meta_f = try pr.Client.init(gpa, fh.face(), 1, 0x4444);
    defer meta_f.deinit();
    // The data namespace: reserved WEAR by O, the superseded manager, still
    // registered there.
    var dns = fake.Namespace.init(gpa);
    defer dns.deinit();
    var doh: fake.Host = .{ .ns = &dns, .host_id = o_id };
    var data_o = try pr.Client.init(gpa, doh.face(), 1, 0x3333);
    defer data_o.deinit();
    data_o.rtype = nvme.RTYPE_WEAR;
    _ = try data_o.registerLadder();
    try data_o.acquire();
    try testing.expect(!dns.writeAllowed(m_id));
    const local = proc.localIdentity();
    var no_pages: u8 = 0;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    // M's admission (volume 0's writer): registered, the reservation kept,
    // O's key preempted (absent from the metadata namespace).
    const reconciles0 = stats.data_pr_reconcile_preempts.load();
    var dm: DataFence = .init(.{ .gpa = gpa, .env = &env, .meta = &meta_m, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = true });
    defer dm.release();
    var dmh: fake.Host = .{ .ns = &dns, .host_id = m_id };
    try dm.admitClient(1, "fake-data", null, try fixtureClient(gpa, dmh.face()));
    try testing.expect(dns.writeAllowed(m_id));
    try testing.expect(!dns.writeAllowed(o_id));
    try testing.expectEqual(nvme.RTYPE_WEAR, dns.rtype);
    try testing.expectEqual(reconciles0 + 1, stats.data_pr_reconcile_preempts.load());
    // J (a joiner): refused by the device before its admission, admitted after.
    var djh: fake.Host = .{ .ns = &dns, .host_id = j_id };
    try testing.expect(!dns.writeAllowed(j_id));
    var dj: DataFence = .init(.{ .gpa = gpa, .env = &env, .meta = &meta_j, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = false });
    defer dj.release();
    try dj.admitClient(1, "fake-data", null, try fixtureClient(gpa, djh.face()));
    try testing.expect(dns.writeAllowed(j_id));
    // A second process of host J adopts: nothing on the device moves.
    const gen = dns.generation;
    const commands_before = dns.commands;
    var dj2: DataFence = .init(.{ .gpa = gpa, .env = &env, .meta = &meta_j, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = false });
    defer dj2.release();
    try dj2.admitClient(1, "fake-data", null, try fixtureClient(gpa, djh.face()));
    try testing.expectEqual(gen, dns.generation);
    try testing.expect(dns.commands > commands_before);
    // Host F is not registered on the metadata namespace (fenced): refused,
    // never registered on data.
    var dff: DataFence = .init(.{ .gpa = gpa, .env = &env, .meta = &meta_f, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = false });
    defer dff.release();
    var dfh: fake.Host = .{ .ns = &dns, .host_id = f_id };
    try testing.expectError(error.HostFenced, dff.admitClient(1, "fake-data", null, try fixtureClient(gpa, dfh.face())));
    try testing.expect(!dns.writeAllowed(f_id));
    try testing.expect(!dns.isRegistered(0x4444));
    // An unreserved namespace: the first registrant reserves WEAR.
    var dns2 = fake.Namespace.init(gpa);
    defer dns2.deinit();
    var d2h: fake.Host = .{ .ns = &dns2, .host_id = j_id };
    try dj.admitClient(2, "fake-data-2", null, try fixtureClient(gpa, d2h.face()));
    try testing.expectEqual(nvme.RTYPE_WEAR, dns2.rtype);
    try testing.expect(!dns2.writeAllowed(m_id));
}
