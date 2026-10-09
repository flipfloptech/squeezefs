//! The driver of the SDK's C smoke (`zig build sdk-smoke`, in the gate on
//! the Linux venue): hosts the in-process stub daemon of `ipc/client.zig`
//! on a scratch "mount" directory, arms the `SQUEEZEFS_TEST_SDK_IL0_BLOB`
//! seam, and calls the C program's two scenarios — the surface end to end
//! against `libsqueezefs_sdk.a`, then the daemon's death. No daemon, no
//! FUSE mount, no root. Exits 0 on success; the C side names the failing
//! line.

const std = @import("std");
const squeezefs = @import("squeezefs");
const sdk_session = squeezefs.sdk.session;
const layout = squeezefs.ipc.session.layout;

extern fn c_smoke_run(mount_dir: [*:0]const u8, file_path: [*:0]const u8) c_int;
extern fn c_smoke_death(mount_dir: [*:0]const u8, file_path: [*:0]const u8) c_int;

var fixture: sdk_session.Fixture = undefined;

fn sleepNs(ns: u64) void {
    const ts: std.os.linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
    _ = std.os.linux.nanosleep(&ts, null);
}

/// The C program's kill switch: the stub stops stamping its heartbeat and
/// severs its socket on its last pass.
export fn smoke_host_kill() callconv(.c) void {
    fixture.host.sever.store(true, .release);
    fixture.host.frozen.store(true, .release);
    sleepNs(20 * std.time.ns_per_ms);
}

pub fn main() !void {
    const gpa = std.heap.c_allocator;
    // The default ring and window; a 16 MiB default arena the first scenario
    // asks to double (32 MiB) under a 4 GiB cap; a 32 MiB file image.
    const geometry: layout.Geometry = .{ .ring_entries = 256, .slots = 256, .arena_bytes = 16 << 20, .max_op_bytes = 1 << 20 };
    try fixture.init(gpa, geometry, 32 << 20);
    defer fixture.deinit(gpa);
    fixture.armSeam();
    try fixture.acceptNext();
    const rc = c_smoke_run(fixture.dir().ptr, fixture.file().ptr);
    fixture.joinAcceptor();
    if (rc != 0) {
        std.debug.print("sdk c smoke: the surface scenario failed at c_smoke.c:{d}\n", .{rc});
        std.process.exit(1);
    }
    fixture.resetHost();
    try fixture.acceptNext();
    const rc2 = c_smoke_death(fixture.dir().ptr, fixture.file().ptr);
    fixture.joinAcceptor();
    if (rc2 != 0) {
        std.debug.print("sdk c smoke: the death scenario failed at c_smoke.c:{d}\n", .{rc2});
        std.process.exit(1);
    }
    std.debug.print("sdk c smoke: ok (surface + death against the in-process stub host)\n", .{});
}
