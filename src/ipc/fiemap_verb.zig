//! The FIEMAP verb (charter §4 "FIEMAP", decision 0003 §CJ): the one ctl
//! round trip the interposer's `ioctl(FS_IOC_FIEMAP)` hook makes, because
//! Linux answers FIEMAP on a FUSE file in the VFS and never asks the
//! daemon. `FiemapReq` carries the caller's `struct fiemap` fields and
//! EXACTLY ONE fd — the very description the app passed to `ioctl`, so no
//! binding is needed (an unbound fd of the mount is served the same) and
//! the daemon judges the file by its own `fstat`, as the fd screen does.
//! The reply is ONE datagram: `FiemapReply{errno, fm_flags,
//! fm_mapped_extents, n}` ‖ `n` × `struct fiemap_extent` (≤
//! `REPLY_MAX_EXTENTS`); `errno` 0 = served. A caller wanting more extents
//! than one datagram carries asks again from the last extent's end.
//!
//! Lives beside the wire rather than in it so the one shared table (the
//! tags) is the only line two verbs' authors both touch. Linux only: the
//! client half speaks raw `recvmsg`/`poll` through `rendezvous_linux`.

const std = @import("std");
const linux = std.os.linux;

const wire = @import("wire.zig");
const rendezvous = @import("rendezvous.zig");
const rl = @import("rendezvous_linux.zig");
const client = @import("client.zig");
const clock = @import("../core/clock.zig");
const fuzzkit = @import("../core/fuzzkit.zig");
pub const fiemap = @import("../fuse/handler/fiemap.zig");

pub const Fiemap = fiemap.Fiemap;
pub const Extent = fiemap.Extent;

/// client → daemon, with exactly ONE fd (the description `ioctl` named).
pub const FiemapReq = extern struct {
    tag: u32 = @backingInt(wire.Tag.fiemap_req),
    fm_flags: u32,
    fm_start: u64,
    fm_length: u64,
    fm_extent_count: u32,
    _pad: u32 = 0,
};

/// daemon → client: the header, then `n_extents` extents.
pub const FiemapReplyHeader = extern struct {
    tag: u32 = @backingInt(wire.Tag.fiemap_reply),
    /// 0 = served; else the kernel errno `ioctl` answers.
    errno: u32,
    /// The `fm_flags` the caller reads back (EBADR: the refused bits).
    fm_flags: u32,
    fm_mapped_extents: u32,
    n_extents: u32,
    _pad: u32 = 0,
};

comptime {
    std.debug.assert(@sizeOf(FiemapReq) == 32);
    std.debug.assert(@sizeOf(FiemapReplyHeader) == 24);
    std.debug.assert(@sizeOf(FiemapReplyHeader) + REPLY_MAX_EXTENTS * @sizeOf(Extent) <= wire.CTL_MSG_MAX);
}

/// The extents one reply datagram carries.
pub const REPLY_MAX_EXTENTS: u32 = (wire.CTL_MSG_MAX - @sizeOf(FiemapReplyHeader)) / @sizeOf(Extent);

/// The largest errno the kernel's `-errno` convention can carry.
const ERRNO_MAX: u32 = 4095;

pub const DecodeError = wire.DecodeError;

pub fn decodeReq(raw: []const u8) DecodeError!FiemapReq {
    if (raw.len != @sizeOf(FiemapReq)) return error.Truncated;
    var v: FiemapReq = undefined;
    @memcpy(std.mem.asBytes(&v), raw);
    if (v.tag != @backingInt(wire.Tag.fiemap_req)) return error.BadTag;
    if (v._pad != 0) return error.BadString;
    return v;
}

/// The `struct fiemap` a request names (the header `prep` judges).
pub fn header(r: FiemapReq) Fiemap {
    return .{ .fm_start = r.fm_start, .fm_length = r.fm_length, .fm_flags = r.fm_flags, .fm_mapped_extents = 0, .fm_extent_count = r.fm_extent_count, .fm_reserved = 0 };
}

pub const Reply = struct {
    errno: u32,
    fm_flags: u32,
    fm_mapped_extents: u32,
    /// Viewed in the datagram (byte-aligned: the receive buffer is bytes).
    extents: []align(1) const Extent,
};

/// Total: the count is capped and must size the datagram exactly; a served
/// reply carries no more extents than it says it mapped; an error reply
/// carries none.
pub fn decodeReply(raw: []const u8) DecodeError!Reply {
    if (raw.len < @sizeOf(FiemapReplyHeader)) return error.Truncated;
    var h: FiemapReplyHeader = undefined;
    @memcpy(std.mem.asBytes(&h), raw[0..@sizeOf(FiemapReplyHeader)]);
    if (h.tag != @backingInt(wire.Tag.fiemap_reply)) return error.BadTag;
    if (h._pad != 0 or h.errno > ERRNO_MAX or h.n_extents > REPLY_MAX_EXTENTS) return error.BadString;
    if (h.n_extents > h.fm_mapped_extents or (h.errno != 0 and h.n_extents != 0)) return error.BadString;
    if (raw.len != @sizeOf(FiemapReplyHeader) + @as(usize, h.n_extents) * @sizeOf(Extent)) return error.Truncated;
    return .{ .errno = h.errno, .fm_flags = h.fm_flags, .fm_mapped_extents = h.fm_mapped_extents, .extents = std.mem.bytesAsSlice(Extent, raw[@sizeOf(FiemapReplyHeader)..]) };
}

/// A refusal: the header alone.
pub fn refusal(errno: linux.E, fm_flags: u32) FiemapReplyHeader {
    return .{ .errno = @backingInt(errno), .fm_flags = fm_flags, .fm_mapped_extents = 0, .n_extents = 0 };
}

/// The daemon's screen of the request's fd: an open (not `O_PATH`) regular
/// file of the mount — `ioctl` itself needs no access mode, so none is
/// asked, and an `O_APPEND`/`O_SYNC`/unlinked description is a file with a
/// layout like any other. The nodeid, or null (EOPNOTSUPP: what Linux
/// answers for a FUSE file that is not served).
pub fn screen(f: rendezvous.FdFacts, expected_dev: ?u64) ?u64 {
    const fl = f.flags orelse return null;
    if (fl.path) return null;
    const st = f.stat orelse return null;
    if (st.kind != .regular) return null;
    const dev = expected_dev orelse return null;
    if (st.dev != dev) return null;
    return st.ino;
}

// ---------------------------------------------------------------------------
// The client half.
// ---------------------------------------------------------------------------

pub const AskError = error{
    /// The session is poisoned, or the ctl socket died: the real call.
    Poisoned,
    /// A malformed reply (the session is poisoned).
    Protocol,
};

/// The liveness slice of the reply wait (the ctl socket's own `SO_RCVTIMEO`
/// bounds a handshake, not a FIEMAP whose SYNC lands a write cache).
const WAIT_SLICE_MS: i32 = @intCast(rendezvous.CTL_RECV_POLL_NS / std.time.ns_per_ms);

/// One round trip under the ctl lock: the request with `fd`, then the reply
/// into `buf` (`wire.CTL_MSG_MAX` bytes, the caller's). Waits as long as the
/// daemon lives (heartbeat), as a FUSE request does.
pub fn ask(s: *client.Session, fd: linux.fd_t, req: FiemapReq, buf: []u8) AskError!Reply {
    std.debug.assert(buf.len >= wire.CTL_MSG_MAX);
    if (s.poisoned()) return error.Poisoned;
    s.ctl_lock.lock();
    defer s.ctl_lock.unlock();
    rl.sendCtl(s.ctl, std.mem.asBytes(&req), fd) catch {
        s.poison();
        return error.Poisoned;
    };
    while (true) {
        const got = rl.tryRecvCtl(s.ctl, buf) catch {
            s.poison();
            return error.Poisoned;
        };
        if (got) |r| {
            if (r.fd) |f| {
                _ = linux.close(f);
                s.poison();
                return error.Protocol;
            }
            return decodeReply(buf[0..r.len]) catch {
                s.poison();
                return error.Protocol;
            };
        }
        var pfd = [_]linux.pollfd{.{ .fd = s.ctl, .events = linux.POLL.IN, .revents = 0 }};
        _ = linux.poll(&pfd, 1, WAIT_SLICE_MS);
        if (pfd[0].revents == 0 and !s.heartbeatAlive(clock.monotonicNs())) {
            s.poison();
            return error.Poisoned;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the FIEMAP verb's frames: a 32 B request, a 24 B reply header, 1,169 extents per datagram; the request round-trips its struct fiemap; a reply decodes only at its exact length, never more extents than mapped, an error with none" {
    try testing.expectEqual(@as(u32, 1169), REPLY_MAX_EXTENTS);
    const req: FiemapReq = .{ .fm_flags = fiemap.FIEMAP_FLAG_SYNC, .fm_start = 4096, .fm_length = 1 << 30, .fm_extent_count = 32 };
    const back = try decodeReq(std.mem.asBytes(&req));
    try testing.expectEqual(req, back);
    const h = header(back);
    try testing.expectEqual(@as(u64, 4096), h.fm_start);
    try testing.expectEqual(@as(u32, 32), h.fm_extent_count);
    try testing.expectError(error.Truncated, decodeReq(std.mem.asBytes(&req)[0..31]));
    var bad = req;
    bad.tag = @backingInt(wire.Tag.bind);
    try testing.expectError(error.BadTag, decodeReq(std.mem.asBytes(&bad)));

    var frame: [@sizeOf(FiemapReplyHeader) + 2 * @sizeOf(Extent)]u8 = undefined;
    const rh: FiemapReplyHeader = .{ .errno = 0, .fm_flags = 0, .fm_mapped_extents = 2, .n_extents = 2 };
    @memcpy(frame[0..@sizeOf(FiemapReplyHeader)], std.mem.asBytes(&rh));
    const e: Extent = .{ .fe_logical = 0, .fe_physical = 1 << 48, .fe_length = 4 << 20, .fe_reserved64 = @splat(0), .fe_flags = fiemap.FIEMAP_EXTENT_LAST, .fe_reserved = @splat(0) };
    @memcpy(frame[@sizeOf(FiemapReplyHeader)..][0..@sizeOf(Extent)], std.mem.asBytes(&e));
    @memcpy(frame[@sizeOf(FiemapReplyHeader) + @sizeOf(Extent) ..][0..@sizeOf(Extent)], std.mem.asBytes(&e));
    const r = try decodeReply(&frame);
    try testing.expectEqual(@as(usize, 2), r.extents.len);
    try testing.expectEqual(@as(u64, 4 << 20), r.extents[1].fe_length);
    try testing.expectError(error.Truncated, decodeReply(frame[0 .. frame.len - 1]));
    var over = rh;
    over.fm_mapped_extents = 1;
    @memcpy(frame[0..@sizeOf(FiemapReplyHeader)], std.mem.asBytes(&over));
    try testing.expectError(error.BadString, decodeReply(&frame));
    const ref = refusal(.BADR, fiemap.FIEMAP_FLAG_XATTR);
    const rr = try decodeReply(std.mem.asBytes(&ref));
    try testing.expectEqual(@as(u32, @backingInt(linux.E.BADR)), rr.errno);
    try testing.expectEqual(fiemap.FIEMAP_FLAG_XATTR, rr.fm_flags);
    var err_with = rh;
    err_with.errno = @backingInt(linux.E.IO);
    @memcpy(frame[0..@sizeOf(FiemapReplyHeader)], std.mem.asBytes(&err_with));
    try testing.expectError(error.BadString, decodeReply(&frame));
}

test "the FIEMAP verb's screen: an open regular file of the mount, any access mode (O_WRONLY, O_APPEND, unlinked included) — O_PATH, a directory, another device, an unreadable fd refuse" {
    const ok: rendezvous.FdFacts = .{ .flags = .{ .accmode = .wronly, .append = true }, .stat = .{ .kind = .regular, .dev = 7, .ino = 42, .nlink = 0 } };
    try testing.expectEqual(@as(?u64, 42), screen(ok, 7));
    try testing.expectEqual(@as(?u64, null), screen(ok, 8));
    try testing.expectEqual(@as(?u64, null), screen(ok, null));
    var p = ok;
    p.flags.?.path = true;
    try testing.expectEqual(@as(?u64, null), screen(p, 7));
    var d = ok;
    d.stat.?.kind = .directory;
    try testing.expectEqual(@as(?u64, null), screen(d, 7));
    try testing.expectEqual(@as(?u64, null), screen(.{ .flags = null, .stat = ok.stat }, 7));
    try testing.expectEqual(@as(?u64, null), screen(.{ .flags = ok.flags, .stat = null }, 7));
}

fn replyFuzz(_: void, smith: *std.testing.Smith) !void {
    var raw: [@sizeOf(FiemapReplyHeader) + 4 * @sizeOf(Extent)]u8 = undefined;
    const len = smith.slice(&raw);
    if (decodeReq(raw[0..len])) |r| try testing.expectEqual(@as(u32, 0), r._pad) else |_| {}
    const r = decodeReply(raw[0..len]) catch return;
    try testing.expect(r.extents.len <= r.fm_mapped_extents and r.errno <= ERRNO_MAX);
    try testing.expectEqual(len, @sizeOf(FiemapReplyHeader) + r.extents.len * @sizeOf(Extent));
}

test "fuzz: the FIEMAP verb's decoders are total on arbitrary bytes" {
    try fuzzkit.fuzz({}, replyFuzz, .{});
}
