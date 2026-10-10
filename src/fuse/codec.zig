//! The FUSE request/reply codec (`docs/porting/fuse_abi_session.md` §2, §5,
//! §8; `fuse_handlers_lanes.md` §1.2, §1.4; charter §4, §12).
//!
//! Requests arrive as `fuse_in_header ‖ in_args[0] ‖ in_args[1..]`: on the
//! classical device the three are one contiguous read; on the ring the
//! kernel places `in_args[0]` (the per-op struct) in the ent's 128-byte
//! `op_in` region and `in_args[1..]` (names, xattr values, WRITE data, the
//! FORGET array) in the payload buffer. Both venues become ONE `Delivery
//! {header, op_in, payload}` and ONE `parse` per opcode (no re-serialization,
//! no heuristic — decision 0002 ledger, fuse_abi_session.md Q4). Opcodes
//! whose `in_args[0]` is the kernel's zero-size placeholder (`fuse_set_zero_
//! arg0`: LOOKUP, SYMLINK, UNLINK, RMDIR, REMOVEXATTR, NOTIFY_REPLY) have an
//! empty `op_in` and read everything from the payload, identically on both
//! venues.
//!
//! The FUSE-3g body-bounds law (`validatedBody`): `fuse_in_header.len` is
//! kernel-supplied and NEVER an allocation or slicing authority — below 40
//! the historical subtraction underflowed to a ~2^64 slice, above `40 +
//! filled` the slice covered the PREVIOUS request's bytes still in the
//! reused buffer. A refused body is a counted `-EINVAL`; a short read has no
//! `unique` to address and is dropped.
//!
//! Replies are `fuse_out_header ‖ out-struct [‖ body]` with `len` covering
//! everything; an error reply is the 16-byte header alone with `error =
//! -errno` — ONE `errorHeader` that always negates through the one errno
//! table (Q6: the reference replied a POSITIVE EINVAL on an INIT decode
//! failure, which the kernel itself refuses). The `Reply` value carries the
//! header, the out-struct bytes (in a caller-owned `OutBuf`) and a zero-copy
//! body VIEW, so READ/READLINK/xattr/dirent bodies are never copied by the
//! codec; the transport places them in iovecs or the ent payload. An oversize
//! reply (`out + body > payload cap`) is the caller's header-only `-EIO`
//! (`Reply.oversize`), never a truncated body under a header claiming the
//! full length.
//!
//! Timestamps are nanosecond-exact (Q2): ONE `fuse_entry_out` encoder with
//! SEPARATE entry and attr TTLs (Q1) serves LOOKUP, CREATE, READDIRPLUS and
//! negative entries. Notify frames compute `len` from the bytes they emit and
//! carry the trailing NUL the kernel requires (Q13).

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const linux = std.os.linux;
const abi = @import("abi.zig");
const geometry = @import("geometry.zig");
const errors = @import("../core/errors.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const record = @import("../meta/record.zig");

pub const HEADER_LEN: usize = @sizeOf(abi.fuse_in_header);
pub const OUT_HEADER_LEN: usize = @sizeOf(abi.fuse_out_header);
const ns_per_s: u64 = std.time.ns_per_s;

/// `fuse_requests_malformed`: bodies refused by the bounds law or a per-op
/// parse (answered -EINVAL, or dropped when no reply is owed). A kernel never
/// produces these; growth names a transport bug.
pub var requests_malformed: counters.ShardedCounter = .{};
/// `fuse_lookup_negative_replies`: negative entries encoded (nodeid 0 +
/// entry TTL) — the cacheable ENOENT.
pub var lookup_negative_replies: counters.ShardedCounter = .{};

// ---------------------------------------------------------------------------
// The opcode table: the kernel's `in_args[0].size` per opcode and whether a
// reply is owed. Null = not served (SETUPMAPPING…STATX and anything past
// 7.45): header-only -ENOSYS, and the kernel latches `fc->no_<op>`.
// ---------------------------------------------------------------------------

pub const OpInfo = struct {
    /// `in_args[0].size`: the per-op struct, 0 for the zero-arg0 opcodes.
    in_size: u8,
    /// FORGET / BATCH_FORGET / INTERRUPT / NOTIFY_REPLY: the kernel clears
    /// `FR_ISREPLY`; a reply would be refused (-ENOENT) and is never sent.
    no_reply: bool = false,
};

pub fn opInfo(op: abi.Opcode) ?OpInfo {
    return switch (op) {
        .lookup => .{ .in_size = 0 },
        .forget => .{ .in_size = @sizeOf(abi.fuse_forget_in), .no_reply = true },
        .getattr => .{ .in_size = @sizeOf(abi.fuse_getattr_in) },
        .setattr => .{ .in_size = @sizeOf(abi.fuse_setattr_in) },
        .readlink => .{ .in_size = 0 },
        .symlink => .{ .in_size = 0 },
        .mknod => .{ .in_size = @sizeOf(abi.fuse_mknod_in) },
        .mkdir => .{ .in_size = @sizeOf(abi.fuse_mkdir_in) },
        .unlink, .rmdir => .{ .in_size = 0 },
        .rename => .{ .in_size = @sizeOf(abi.fuse_rename_in) },
        .link => .{ .in_size = @sizeOf(abi.fuse_link_in) },
        .open, .opendir => .{ .in_size = @sizeOf(abi.fuse_open_in) },
        .read, .readdir, .readdirplus => .{ .in_size = @sizeOf(abi.fuse_read_in) },
        .write => .{ .in_size = @sizeOf(abi.fuse_write_in) },
        .statfs => .{ .in_size = 0 },
        .release, .releasedir => .{ .in_size = @sizeOf(abi.fuse_release_in) },
        .fsync, .fsyncdir => .{ .in_size = @sizeOf(abi.fuse_fsync_in) },
        .setxattr => .{ .in_size = @sizeOf(abi.fuse_setxattr_in) },
        .getxattr, .listxattr => .{ .in_size = @sizeOf(abi.fuse_getxattr_in) },
        .removexattr => .{ .in_size = 0 },
        .flush => .{ .in_size = @sizeOf(abi.fuse_flush_in) },
        .init => .{ .in_size = @sizeOf(abi.fuse_init_in) },
        .getlk, .setlk, .setlkw => .{ .in_size = @sizeOf(abi.fuse_lk_in) },
        .access => .{ .in_size = @sizeOf(abi.fuse_access_in) },
        .create => .{ .in_size = @sizeOf(abi.fuse_create_in) },
        .interrupt => .{ .in_size = @sizeOf(abi.fuse_interrupt_in), .no_reply = true },
        .bmap => .{ .in_size = @sizeOf(abi.fuse_bmap_in) },
        .destroy => .{ .in_size = 0 },
        .ioctl => .{ .in_size = @sizeOf(abi.fuse_ioctl_in) },
        .poll => .{ .in_size = @sizeOf(abi.fuse_poll_in) },
        .notify_reply => .{ .in_size = 0, .no_reply = true },
        .batch_forget => .{ .in_size = @sizeOf(abi.fuse_batch_forget_in), .no_reply = true },
        .fallocate => .{ .in_size = @sizeOf(abi.fuse_fallocate_in) },
        .rename2 => .{ .in_size = @sizeOf(abi.fuse_rename2_in) },
        .lseek => .{ .in_size = @sizeOf(abi.fuse_lseek_in) },
        .copy_file_range => .{ .in_size = @sizeOf(abi.fuse_copy_file_range_in) },
        .setupmapping, .removemapping, .syncfs, .tmpfile, .statx, _ => null,
    };
}

// ---------------------------------------------------------------------------
// Body bounds (FUSE-3g) and the delivery triple
// ---------------------------------------------------------------------------

pub const BodyBounds = union(enum) {
    /// `min(declared, filled, buf_len)` bytes of body.
    valid: usize,
    /// `len < 40`: nothing after the header can be trusted.
    short_header,
    /// `declared > filled + payload_len`: the slice would read stale bytes.
    overdeclared: struct { declared: usize, available: usize },
};

/// `declared = len − 40` (underflow ⇒ short); `available = filled +
/// payload_len` (`payload_len` = an out-of-band WRITE lease length, else 0);
/// `declared > available ⇒ overdeclared`; else `min(declared, filled, buf_len)`.
pub fn validatedBody(header_len: u32, filled: usize, payload_len: usize, buf_len: usize) BodyBounds {
    if (header_len < HEADER_LEN) return .short_header;
    const declared: usize = header_len - HEADER_LEN;
    const available = filled + payload_len;
    if (declared > available) return .{ .overdeclared = .{ .declared = declared, .available = available } };
    return .{ .valid = @min(declared, @min(filled, buf_len)) };
}

/// ONE shape for both venues.
pub const Delivery = struct {
    header: abi.fuse_in_header,
    /// `in_args[0]`: exactly the opcode's struct bytes (empty for the
    /// zero-arg0 opcodes); SHORTER than the struct only on a malformed
    /// classical body, which `parse` refuses.
    op_in: []const u8,
    /// `in_args[1..]` in order.
    payload: []const u8,
    /// Bytes of a WRITE body that sit in a zero-copy slot instead of
    /// `payload` (the kernel announced them; nothing was copied): the parse
    /// admits `in.size == held_len` with an empty `data` view.
    held_len: usize = 0,

    pub const ClassicalError = error{ ShortRead, ShortHeader, Overdeclared };

    /// `buf` = the device read buffer, `n` = what the read returned (header
    /// included). The body is bounded by the law above and split at the
    /// opcode's `in_args[0].size`.
    pub fn fromClassical(buf: []const u8, n: usize) ClassicalError!Delivery {
        if (n < HEADER_LEN or buf.len < HEADER_LEN) return error.ShortRead;
        const header = abi.decodePrefix(abi.fuse_in_header, buf).?;
        const filled = @min(n, buf.len) - HEADER_LEN;
        const body_len = switch (validatedBody(header.len, filled, 0, buf.len - HEADER_LEN)) {
            .valid => |v| v,
            .short_header => {
                requests_malformed.inc();
                return error.ShortHeader;
            },
            .overdeclared => {
                requests_malformed.inc();
                return error.Overdeclared;
            },
        };
        const body = buf[HEADER_LEN..][0..body_len];
        const split = @min(inSizeOf(header.op()), body.len);
        return .{ .header = header, .op_in = body[0..split], .payload = body[split..] };
    }

    pub const RingError = error{ ShortHeader, Overdeclared };

    /// `in_out` and `op_in` are the ent's two 128-byte regions; `payload` the
    /// delivered bytes (`min(ring_ent_in_out.payload_sz, ent payload len)`);
    /// `payload_len` the authoritative body length when it rides elsewhere
    /// (a held zero-copy slot presents an empty placeholder: the difference
    /// is `held_len`), else `payload.len`. The op_in region past
    /// `in_args[0].size` holds stale bytes from earlier deliveries and is
    /// never exposed.
    pub fn fromRing(in_out: *const [abi.FUSE_URING_IN_OUT_HEADER_SZ]u8, op_in: *const [abi.FUSE_URING_OP_IN_OUT_SZ]u8, payload: []const u8, payload_len: usize) RingError!Delivery {
        const header = abi.decodePrefix(abi.fuse_in_header, in_out).?;
        switch (validatedBody(header.len, op_in.len, payload_len, op_in.len + payload_len)) {
            .valid => {},
            .short_header => {
                requests_malformed.inc();
                return error.ShortHeader;
            },
            .overdeclared => {
                requests_malformed.inc();
                return error.Overdeclared;
            },
        }
        return .{ .header = header, .op_in = op_in[0..inSizeOf(header.op())], .payload = payload, .held_len = payload_len -| payload.len };
    }

    fn inSizeOf(op: abi.Opcode) usize {
        return if (opInfo(op)) |i| i.in_size else 0;
    }
};

// ---------------------------------------------------------------------------
// Per-op requests
// ---------------------------------------------------------------------------

pub const ParseError = error{
    /// The body is shorter than the opcode's struct.
    ShortOpIn,
    /// A name without its NUL terminator.
    MissingNul,
    /// A `size`/`count` field reaching past the payload.
    ShortPayload,
    /// `total_extlen != 0` with no extension negotiated (Q14: the padding
    /// word is a capability; a non-zero value is a kernel we do not know).
    UnexpectedExtension,
};

/// Every parse refusal is `-EINVAL` on the wire (for a reply-owing opcode).
pub fn parseErrno(_: ParseError) linux.E {
    return .INVAL;
}

pub const Name = struct { name: []const u8 };
pub const Forget = struct { nlookup: u64 };
pub const Getattr = struct {
    flags: u32,
    /// Present iff `FUSE_GETATTR_FH`.
    fh: ?u64,
};
pub const Symlink = struct { name: []const u8, link: []const u8 };
pub const Mknod = struct { name: []const u8, mode: u32, rdev: u32, umask: u32 };
pub const Mkdir = struct { name: []const u8, mode: u32, umask: u32 };
/// RENAME (`flags = 0`) and RENAME2.
pub const Rename = struct { newdir: u64, flags: u32, name: []const u8, newname: []const u8 };
pub const Link = struct { oldnodeid: u64, newname: []const u8 };
pub const Open = struct {
    flags: u32,
    /// `FUSE_OPEN_KILL_SUIDGID` on an O_TRUNC open the daemon must strip.
    open_flags: u32,
};
pub const Write = struct {
    in: abi.fuse_write_in,
    /// Exactly `in.size` bytes: on the ring a zero-copy view of the leased
    /// payload buffer — EMPTY for a HELD zero-copy payload (`in.size > 0`,
    /// the bytes sit in the slot: `PayloadLease.held`, `heldSource` /
    /// `extract`).
    data: []const u8,
};
pub const Fsync = struct { fh: u64, datasync: bool };
pub const Setxattr = struct { name: []const u8, value: []const u8, flags: u32 };
pub const Getxattr = struct { name: []const u8, size: u32 };
pub const Listxattr = struct { size: u32 };
pub const Flush = struct { fh: u64, lock_owner: u64 };
pub const Init = struct {
    in: abi.fuse_init_in,
    /// The classical body length the kernel sent (16 or 64).
    raw_len: usize,
};
pub const Access = struct { mask: u32 };
pub const Create = struct { name: []const u8, flags: u32, mode: u32, umask: u32, open_flags: u32 };
pub const Interrupt = struct { unique: u64 };
pub const Bmap = struct { block: u64, blocksize: u32 };
pub const Ioctl = struct { in: abi.fuse_ioctl_in, data: []const u8 };
pub const NotifyReply = struct { offset: u64, data: []const u8 };
pub const BatchForget = struct { entries: []align(1) const abi.fuse_forget_one };
pub const Lseek = struct { fh: u64, offset: u64, whence: u32 };

pub const Request = union(enum) {
    lookup: Name,
    forget: Forget,
    getattr: Getattr,
    setattr: abi.fuse_setattr_in,
    readlink,
    symlink: Symlink,
    mknod: Mknod,
    mkdir: Mkdir,
    unlink: Name,
    rmdir: Name,
    rename: Rename,
    link: Link,
    open: Open,
    read: abi.fuse_read_in,
    write: Write,
    statfs,
    release: abi.fuse_release_in,
    fsync: Fsync,
    setxattr: Setxattr,
    getxattr: Getxattr,
    listxattr: Listxattr,
    removexattr: Name,
    flush: Flush,
    init: Init,
    opendir: Open,
    readdir: abi.fuse_read_in,
    releasedir: abi.fuse_release_in,
    fsyncdir: Fsync,
    getlk: abi.fuse_lk_in,
    setlk: abi.fuse_lk_in,
    setlkw: abi.fuse_lk_in,
    access: Access,
    create: Create,
    interrupt: Interrupt,
    bmap: Bmap,
    destroy,
    ioctl: Ioctl,
    poll: abi.fuse_poll_in,
    notify_reply: NotifyReply,
    batch_forget: BatchForget,
    fallocate: abi.fuse_fallocate_in,
    readdirplus: abi.fuse_read_in,
    rename2: Rename,
    lseek: Lseek,
    copy_file_range: abi.fuse_copy_file_range_in,
    /// Known past what we serve, or unknown: header-only -ENOSYS.
    unsupported: abi.Opcode,
};

fn opIn(comptime T: type, d: Delivery) ParseError!T {
    return abi.decodePrefix(T, d.op_in) orelse error.ShortOpIn;
}

const Split = struct { name: []const u8, rest: []const u8 };

fn takeName(bytes: []const u8) ParseError!Split {
    const nul = std.mem.indexOfScalar(u8, bytes, 0) orelse return error.MissingNul;
    return .{ .name = bytes[0..nul], .rest = bytes[nul + 1 ..] };
}

fn takeBytes(bytes: []const u8, n: usize) ParseError![]const u8 {
    if (bytes.len < n) return error.ShortPayload;
    return bytes[0..n];
}

/// ONE parse per opcode over the delivery triple. Total: every refusal is a
/// `ParseError` (counted), never a trap; trailing payload bytes past the
/// last argument are ignored (a later uapi may append; nothing we negotiate
/// does today).
pub fn parse(d: Delivery) ParseError!Request {
    return parseInner(d) catch |e| {
        requests_malformed.inc();
        return e;
    };
}

fn parseInner(d: Delivery) ParseError!Request {
    if (d.header.total_extlen != 0) return error.UnexpectedExtension;
    const p = d.payload;
    return switch (d.header.op()) {
        .lookup => .{ .lookup = .{ .name = (try takeName(p)).name } },
        .forget => .{ .forget = .{ .nlookup = (try opIn(abi.fuse_forget_in, d)).nlookup } },
        .getattr => blk: {
            const in = try opIn(abi.fuse_getattr_in, d);
            break :blk .{ .getattr = .{ .flags = in.getattr_flags, .fh = if (in.getattr_flags & abi.FUSE_GETATTR_FH != 0) in.fh else null } };
        },
        .setattr => .{ .setattr = try opIn(abi.fuse_setattr_in, d) },
        .readlink => .readlink,
        .symlink => blk: {
            const a = try takeName(p);
            const b = try takeName(a.rest);
            break :blk .{ .symlink = .{ .name = a.name, .link = b.name } };
        },
        .mknod => blk: {
            const in = try opIn(abi.fuse_mknod_in, d);
            break :blk .{ .mknod = .{ .name = (try takeName(p)).name, .mode = in.mode, .rdev = in.rdev, .umask = in.umask } };
        },
        .mkdir => blk: {
            const in = try opIn(abi.fuse_mkdir_in, d);
            break :blk .{ .mkdir = .{ .name = (try takeName(p)).name, .mode = in.mode, .umask = in.umask } };
        },
        .unlink => .{ .unlink = .{ .name = (try takeName(p)).name } },
        .rmdir => .{ .rmdir = .{ .name = (try takeName(p)).name } },
        .rename => blk: {
            const in = try opIn(abi.fuse_rename_in, d);
            const a = try takeName(p);
            const b = try takeName(a.rest);
            break :blk .{ .rename = .{ .newdir = in.newdir, .flags = 0, .name = a.name, .newname = b.name } };
        },
        .rename2 => blk: {
            const in = try opIn(abi.fuse_rename2_in, d);
            const a = try takeName(p);
            const b = try takeName(a.rest);
            break :blk .{ .rename2 = .{ .newdir = in.newdir, .flags = in.flags, .name = a.name, .newname = b.name } };
        },
        .link => blk: {
            const in = try opIn(abi.fuse_link_in, d);
            break :blk .{ .link = .{ .oldnodeid = in.oldnodeid, .newname = (try takeName(p)).name } };
        },
        .open => blk: {
            const in = try opIn(abi.fuse_open_in, d);
            break :blk .{ .open = .{ .flags = in.flags, .open_flags = in.open_flags } };
        },
        .opendir => blk: {
            const in = try opIn(abi.fuse_open_in, d);
            break :blk .{ .opendir = .{ .flags = in.flags, .open_flags = in.open_flags } };
        },
        .read => .{ .read = try opIn(abi.fuse_read_in, d) },
        .readdir => .{ .readdir = try opIn(abi.fuse_read_in, d) },
        .readdirplus => .{ .readdirplus = try opIn(abi.fuse_read_in, d) },
        .write => blk: {
            const in = try opIn(abi.fuse_write_in, d);
            // A held zero-copy body: the bytes sit in the slot; the view is empty.
            const data: []const u8 = if (d.held_len != 0 and p.len == 0 and in.size == d.held_len) &.{} else try takeBytes(p, in.size);
            break :blk .{ .write = .{ .in = in, .data = data } };
        },
        .statfs => .statfs,
        .release => .{ .release = try opIn(abi.fuse_release_in, d) },
        .releasedir => .{ .releasedir = try opIn(abi.fuse_release_in, d) },
        .fsync => blk: {
            const in = try opIn(abi.fuse_fsync_in, d);
            break :blk .{ .fsync = .{ .fh = in.fh, .datasync = in.fsync_flags & abi.FUSE_FSYNC_FDATASYNC != 0 } };
        },
        .fsyncdir => blk: {
            const in = try opIn(abi.fuse_fsync_in, d);
            break :blk .{ .fsyncdir = .{ .fh = in.fh, .datasync = in.fsync_flags & abi.FUSE_FSYNC_FDATASYNC != 0 } };
        },
        .setxattr => blk: {
            const in = try opIn(abi.fuse_setxattr_in, d);
            const a = try takeName(p);
            break :blk .{ .setxattr = .{ .name = a.name, .value = try takeBytes(a.rest, in.size), .flags = in.flags } };
        },
        .getxattr => blk: {
            const in = try opIn(abi.fuse_getxattr_in, d);
            break :blk .{ .getxattr = .{ .name = (try takeName(p)).name, .size = in.size } };
        },
        .listxattr => .{ .listxattr = .{ .size = (try opIn(abi.fuse_getxattr_in, d)).size } },
        .removexattr => .{ .removexattr = .{ .name = (try takeName(p)).name } },
        .flush => blk: {
            const in = try opIn(abi.fuse_flush_in, d);
            break :blk .{ .flush = .{ .fh = in.fh, .lock_owner = in.lock_owner } };
        },
        .init => .{ .init = .{ .in = decodeInitIn(d.op_in) orelse return error.ShortOpIn, .raw_len = d.op_in.len } },
        .getlk => .{ .getlk = try opIn(abi.fuse_lk_in, d) },
        .setlk => .{ .setlk = try opIn(abi.fuse_lk_in, d) },
        .setlkw => .{ .setlkw = try opIn(abi.fuse_lk_in, d) },
        .access => .{ .access = .{ .mask = (try opIn(abi.fuse_access_in, d)).mask } },
        .create => blk: {
            const in = try opIn(abi.fuse_create_in, d);
            break :blk .{ .create = .{ .name = (try takeName(p)).name, .flags = in.flags, .mode = in.mode, .umask = in.umask, .open_flags = in.open_flags } };
        },
        .interrupt => .{ .interrupt = .{ .unique = (try opIn(abi.fuse_interrupt_in, d)).unique } },
        .bmap => blk: {
            const in = try opIn(abi.fuse_bmap_in, d);
            break :blk .{ .bmap = .{ .block = in.block, .blocksize = in.blocksize } };
        },
        .destroy => .destroy,
        .ioctl => blk: {
            const in = try opIn(abi.fuse_ioctl_in, d);
            break :blk .{ .ioctl = .{ .in = in, .data = try takeBytes(p, in.in_size) } };
        },
        .poll => .{ .poll = try opIn(abi.fuse_poll_in, d) },
        .notify_reply => blk: {
            // Zero arg0: `fuse_notify_retrieve_in` leads the payload on both venues.
            const in = abi.decodePrefix(abi.fuse_notify_retrieve_in, p) orelse return error.ShortOpIn;
            break :blk .{ .notify_reply = .{ .offset = in.offset, .data = try takeBytes(p[@sizeOf(abi.fuse_notify_retrieve_in)..], in.size) } };
        },
        .batch_forget => blk: {
            const in = try opIn(abi.fuse_batch_forget_in, d);
            // `count` is kernel-supplied: bounded by the payload, never an allocation authority.
            const bytes = try takeBytes(p, @as(usize, in.count) * @sizeOf(abi.fuse_forget_one));
            break :blk .{ .batch_forget = .{ .entries = std.mem.bytesAsSlice(abi.fuse_forget_one, bytes) } };
        },
        .fallocate => .{ .fallocate = try opIn(abi.fuse_fallocate_in, d) },
        .lseek => blk: {
            const in = try opIn(abi.fuse_lseek_in, d);
            break :blk .{ .lseek = .{ .fh = in.fh, .offset = in.offset, .whence = in.whence } };
        },
        .copy_file_range => .{ .copy_file_range = try opIn(abi.fuse_copy_file_range_in, d) },
        .setupmapping, .removemapping, .syncfs, .tmpfile, .statx, _ => |op| .{ .unsupported = op },
    };
}

/// `fuse_init_in` from the INIT body: the 16-byte prefix is required;
/// `flags2` is read iff `flags & FUSE_INIT_EXT` and the body carried ≥ 20
/// bytes; everything past what the kernel sent reads 0.
pub fn decodeInitIn(body: []const u8) ?abi.fuse_init_in {
    if (body.len < abi.fuse_init_in.CLASSICAL_LEN) return null;
    var in = abi.zeroed(abi.fuse_init_in);
    const take = @min(body.len, @sizeOf(abi.fuse_init_in));
    @memcpy(std.mem.asBytes(&in)[0..take], body[0..take]);
    if (in.flags & abi.flagsOf(abi.FUSE_INIT_EXT) == 0 or body.len < abi.fuse_init_in.FLAGS2_END) in.flags2 = 0;
    return in;
}

// ---------------------------------------------------------------------------
// Time: TTLs (u64 ns) and attribute stamps (i64 ns) split nanosecond-exact.
// ---------------------------------------------------------------------------

pub const SplitTime = struct { sec: u64, nsec: u32 };

/// A TTL in ns → `(sec, nsec)`, exact (Q2: never micros in a nsec field).
pub fn splitTtlNs(ns: u64) SplitTime {
    return .{ .sec = ns / ns_per_s, .nsec = @intCast(ns % ns_per_s) };
}

/// An i64 ns stamp → `(time_t bit pattern, nsec ∈ [0, 1e9))` with floor
/// semantics so a pre-epoch stamp keeps a non-negative nsec.
pub fn splitTimeNs(ns: i64) SplitTime {
    const sec = @divFloor(ns, @as(i64, ns_per_s));
    const nsec = @mod(ns, @as(i64, ns_per_s));
    return .{ .sec = @bitCast(sec), .nsec = @intCast(nsec) };
}

/// The inode value → `fuse_attr`. Times are the value's i64-ns stamps split
/// exact; `rdev` is the kernel's 32-bit new-encoded `dev_t` the value stores
/// verbatim from `fuse_mknod_in.rdev`; `blocks` is the 512-byte-unit size
/// (the rung-4 layouts refine it for packed/sparse files); `flags` 0 (no
/// submount, no DAX).
pub fn attrFromInode(ino: u64, v: record.InodeValue, blksize: u32) abi.fuse_attr {
    const a = splitTimeNs(@bitCast(v.atime));
    const m = splitTimeNs(@bitCast(v.mtime));
    const c = splitTimeNs(@bitCast(v.ctime));
    return .{
        .ino = ino,
        .size = v.size,
        .blocks = derive.ceilDiv(v.size, 512),
        .atime = a.sec,
        .mtime = m.sec,
        .ctime = c.sec,
        .atimensec = a.nsec,
        .mtimensec = m.nsec,
        .ctimensec = c.nsec,
        .mode = v.mode,
        .nlink = v.nlink,
        .uid = v.uid,
        .gid = v.gid,
        .rdev = @truncate(v.rdev),
        .blksize = blksize,
        .flags = 0,
    };
}

/// `fuse_dirent.type` from a mode: the `S_IFMT` nibble.
pub fn direntType(mode: u32) u32 {
    return (mode & 0o170000) >> 12;
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

/// The largest out-struct sequence: CREATE's `fuse_entry_out ‖ fuse_open_out`.
pub const MAX_OUT_LEN: usize = @sizeOf(abi.fuse_entry_out) + @sizeOf(abi.fuse_open_out);
pub const OutBuf = [MAX_OUT_LEN]u8;

comptime {
    std.debug.assert(MAX_OUT_LEN == 144);
    std.debug.assert(@sizeOf(abi.fuse_init_out) <= MAX_OUT_LEN);
    std.debug.assert(@sizeOf(abi.fuse_attr_out) <= MAX_OUT_LEN);
    std.debug.assert(@sizeOf(abi.fuse_statfs_out) <= MAX_OUT_LEN);
}

pub const Reply = struct {
    header: abi.fuse_out_header,
    /// The out-struct bytes (a view into the caller's `OutBuf`).
    out: []const u8,
    /// The body VIEW (READ data, a link target, xattr bytes, packed dirents).
    body: []const u8,
    /// A paged READ's page_zeroing end (`readZeroTail`): the window is
    /// zero past `body.len` up to here, and a zero-copy bounce bridge
    /// moves `zero_pad` bytes into the installed pages (the header still
    /// says `body.len`). 0 = nothing to pad.
    zero_pad: u32 = 0,

    /// What follows the 16-byte header: `out ‖ body`.
    pub fn extraLen(self: Reply) usize {
        return self.out.len + self.body.len;
    }

    pub fn isError(self: Reply) bool {
        return self.header.@"error" != 0;
    }

    /// `extra > the ent's payload capacity` ⇒ the caller commits
    /// `replyError(unique, .IO)` and counts `transport_replies_oversize`
    /// (charter §4: never a truncated body under a header claiming the full
    /// length).
    pub fn oversize(self: Reply, payload_cap: usize) bool {
        return self.extraLen() > payload_cap;
    }
};

pub fn okHeader(unique: u64, extra: usize) abi.fuse_out_header {
    return .{ .len = @intCast(OUT_HEADER_LEN + extra), .@"error" = 0, .unique = unique };
}

/// THE error header: `len 16, error = -errno`. Always negative (Q6).
pub fn errorHeader(unique: u64, errno: linux.E) abi.fuse_out_header {
    return .{ .len = @intCast(OUT_HEADER_LEN), .@"error" = -@as(i32, @intCast(@backingInt(errno))), .unique = unique };
}

pub fn replyError(unique: u64, errno: linux.E) Reply {
    return .{ .header = errorHeader(unique, errno), .out = &.{}, .body = &.{} };
}

/// A typed error through the ONE errno table.
pub fn replyErrorFor(unique: u64, err: errors.Error) Reply {
    return replyError(unique, errors.toErrno(err));
}

/// An opcode the daemon does not serve: the kernel latches `fc->no_<op>`.
pub fn replyEnosys(unique: u64) Reply {
    return replyError(unique, .NOSYS);
}

/// Header-only success (RELEASE, FSYNC, SETXATTR, UNLINK, …).
pub fn replyOk(unique: u64) Reply {
    return .{ .header = okHeader(unique, 0), .out = &.{}, .body = &.{} };
}

fn replyOut(buf: *OutBuf, unique: u64, value: anytype) Reply {
    const n = @sizeOf(@TypeOf(value));
    buf[0..n].* = abi.encode(value);
    return .{ .header = okHeader(unique, n), .out = buf[0..n], .body = &.{} };
}

/// The ONE entry encoder (Q1 + Q2): LOOKUP, CREATE, READDIRPLUS and the
/// negative entry all go through `encode`.
pub const EntryReply = struct {
    nodeid: u64,
    generation: u64 = 0,
    /// The dentry's class TTL (`entry` or `dir_entry`).
    entry_ttl_ns: u64,
    /// The `attr` class TTL — independent of the entry's.
    attr_ttl_ns: u64,
    attr: abi.fuse_attr,

    /// `nodeid 0 + entry_valid`: "same as -ENOENT, but with a valid timeout"
    /// (`fuse_lookup_name`). Attrs are zero and their TTL 0.
    pub fn negative(entry_ttl_ns: u64) EntryReply {
        return .{ .nodeid = 0, .generation = 0, .entry_ttl_ns = entry_ttl_ns, .attr_ttl_ns = 0, .attr = abi.zeroed(abi.fuse_attr) };
    }

    pub fn isNegative(self: EntryReply) bool {
        return self.nodeid == 0;
    }

    pub fn encode(self: EntryReply) abi.fuse_entry_out {
        const e = splitTtlNs(self.entry_ttl_ns);
        const a = splitTtlNs(self.attr_ttl_ns);
        return .{ .nodeid = self.nodeid, .generation = self.generation, .entry_valid = e.sec, .attr_valid = a.sec, .entry_valid_nsec = e.nsec, .attr_valid_nsec = a.nsec, .attr = self.attr };
    }
};

pub const AttrReply = struct {
    attr_ttl_ns: u64,
    attr: abi.fuse_attr,

    /// `dummy` is padding and stays 0 (the reference echoed the request's).
    pub fn encode(self: AttrReply) abi.fuse_attr_out {
        const a = splitTtlNs(self.attr_ttl_ns);
        return .{ .attr_valid = a.sec, .attr_valid_nsec = a.nsec, .dummy = 0, .attr = self.attr };
    }
};

pub const Statfs = struct {
    blocks: u64,
    bfree: u64,
    bavail: u64,
    files: u64,
    ffree: u64,
    bsize: u32,
    namelen: u32,
    frsize: u32,

    pub fn encode(self: Statfs) abi.fuse_statfs_out {
        return .{ .st = .{ .blocks = self.blocks, .bfree = self.bfree, .bavail = self.bavail, .files = self.files, .ffree = self.ffree, .bsize = self.bsize, .namelen = self.namelen, .frsize = self.frsize, .padding = 0, .spare = @splat(0) } };
    }
};

pub fn replyEntry(buf: *OutBuf, unique: u64, e: EntryReply) Reply {
    if (e.isNegative()) lookup_negative_replies.inc();
    return replyOut(buf, unique, e.encode());
}

pub fn replyAttr(buf: *OutBuf, unique: u64, a: AttrReply) Reply {
    return replyOut(buf, unique, a.encode());
}

pub fn replyOpen(buf: *OutBuf, unique: u64, fh: u64, open_flags: u32) Reply {
    return replyOut(buf, unique, abi.fuse_open_out{ .fh = fh, .open_flags = open_flags, .backing_id = 0 });
}

/// CREATE: `fuse_entry_out ‖ fuse_open_out` (144).
pub fn replyCreate(buf: *OutBuf, unique: u64, e: EntryReply, fh: u64, open_flags: u32) Reply {
    const n1 = @sizeOf(abi.fuse_entry_out);
    const n2 = @sizeOf(abi.fuse_open_out);
    buf[0..n1].* = abi.encode(e.encode());
    buf[n1..][0..n2].* = abi.encode(abi.fuse_open_out{ .fh = fh, .open_flags = open_flags, .backing_id = 0 });
    return .{ .header = okHeader(unique, n1 + n2), .out = buf[0 .. n1 + n2], .body = &.{} };
}

/// WRITE and COPY_FILE_RANGE.
pub fn replyWrite(buf: *OutBuf, unique: u64, size: u32) Reply {
    return replyOut(buf, unique, abi.fuse_write_out{ .size = size, .padding = 0 });
}

pub fn replyStatfs(buf: *OutBuf, unique: u64, st: Statfs) Reply {
    return replyOut(buf, unique, st.encode());
}

/// GETXATTR/LISTXATTR with `size == 0`: the size form, a SUCCESS reply
/// (ERANGE for a too-small buffer is the filesystem's error).
pub fn replyXattrSize(buf: *OutBuf, unique: u64, size: u32) Reply {
    return replyOut(buf, unique, abi.fuse_getxattr_out{ .size = size, .padding = 0 });
}

pub fn replyLseek(buf: *OutBuf, unique: u64, offset: u64) Reply {
    return replyOut(buf, unique, abi.fuse_lseek_out{ .offset = offset });
}

pub fn replyBmap(buf: *OutBuf, unique: u64, block: u64) Reply {
    return replyOut(buf, unique, abi.fuse_bmap_out{ .block = block });
}

pub fn replyPoll(buf: *OutBuf, unique: u64, revents: u32) Reply {
    return replyOut(buf, unique, abi.fuse_poll_out{ .revents = revents, .padding = 0 });
}

pub fn replyLk(buf: *OutBuf, unique: u64, lk: abi.fuse_file_lock) Reply {
    return replyOut(buf, unique, abi.fuse_lk_out{ .lk = lk });
}

/// IOCTL: `fuse_ioctl_out [‖ data]`.
pub fn replyIoctl(buf: *OutBuf, unique: u64, out: abi.fuse_ioctl_out, data: []const u8) Reply {
    var r = replyOut(buf, unique, out);
    r.body = data;
    r.header = okHeader(unique, r.out.len + data.len);
    return r;
}

/// Raw bytes: READLINK, xattr data, packed dirents. Zero-copy view.
pub fn replyData(unique: u64, data: []const u8) Reply {
    return .{ .header = okHeader(unique, data.len), .out = &.{}, .body = data };
}

/// READ: the body truncated to `fuse_read_in.size`.
pub fn replyRead(unique: u64, data: []const u8, size: u32) Reply {
    return replyData(unique, data[0..@min(data.len, size)]);
}

/// The kernel's `page_zeroing` for a paged READ (`fuse_do_readfolio`,
/// `fuse_send_readpages`): a SHORT reply leaves every requested page the
/// bytes did not fill ZERO — the classical copy clears each partial page
/// before copying into it. Under zero-copy the daemon's bytes land in
/// the installed pages with no kernel copy, so the daemon zero-completes
/// `window[len .. end)` itself, `end` = the request's size rounded up to
/// the page (the installed pages), bounded by the window. An O_DIRECT
/// read has no page_zeroing (the kernel never zeroes a user buffer past
/// the returned count): its callers skip this. Found by LTP fs gf18: a
/// 1,959-byte inline file read into page 0 left [1959, 4096) as the
/// page's old garbage, marked uptodate; a later 1-byte extend made the
/// garbage readable in the new hole.
pub fn readZeroTail(window: []u8, len: usize, size: u32, page: usize) u32 {
    const end: usize = @intCast(@min(window.len, derive.alignUp(@min(size, window.len), page)));
    if (len < end) @memset(window[len..end], 0);
    return @intCast(@max(end, @min(len, window.len)));
}

// ---------------------------------------------------------------------------
// Directory replies: packed records, each 8-byte aligned and zero-padded,
// stopping when the next record would exceed `fuse_read_in.size`.
// ---------------------------------------------------------------------------

pub const DirentError = error{
    /// The record does not fit: reply what was packed so far.
    Full,
    /// The kernel answers EIO to an empty or over-long name; refuse here.
    EmptyName,
    NameTooLong,
};

fn checkName(name: []const u8) DirentError!void {
    if (name.len == 0) return error.EmptyName;
    if (name.len > abi.FUSE_NAME_MAX) return error.NameTooLong;
}

pub const DirentWriter = struct {
    buf: []u8,
    len: usize = 0,
    /// `min(read_in.size, buf.len)`.
    max: usize,

    pub fn init(buf: []u8, size: u32) DirentWriter {
        return .{ .buf = buf, .max = @min(buf.len, size) };
    }

    /// `next_off` is the cookie the kernel hands back to continue AFTER this
    /// entry; `mode` yields `type = mode >> 12`.
    pub fn add(self: *DirentWriter, ino: u64, next_off: u64, mode: u32, name: []const u8) DirentError!void {
        try checkName(name);
        const rec = abi.fuse_dirent.recordLen(name.len);
        if (self.len + rec > self.max) return error.Full;
        const dst = self.buf[self.len..][0..rec];
        dst[0..abi.fuse_dirent.NAME_OFFSET].* = abi.encode(abi.fuse_dirent{ .ino = ino, .off = next_off, .namelen = @intCast(name.len), .type = direntType(mode) });
        @memcpy(dst[abi.fuse_dirent.NAME_OFFSET..][0..name.len], name);
        @memset(dst[abi.fuse_dirent.NAME_OFFSET + name.len ..], 0);
        self.len += rec;
    }

    pub fn bytes(self: *const DirentWriter) []const u8 {
        return self.buf[0..self.len];
    }
};

pub const DirentPlusWriter = struct {
    buf: []u8,
    len: usize = 0,
    max: usize,

    pub fn init(buf: []u8, size: u32) DirentPlusWriter {
        return .{ .buf = buf, .max = @min(buf.len, size) };
    }

    /// `entry` carries both TTL classes (the ONE encoder); `type` is derived
    /// from `entry.attr.mode`. A `nodeid 0` entry ("." / "..") is legal: the
    /// kernel skips its dentry linkage.
    pub fn add(self: *DirentPlusWriter, entry: EntryReply, next_off: u64, name: []const u8) DirentError!void {
        try checkName(name);
        const rec = abi.fuse_direntplus.recordLen(name.len);
        if (self.len + rec > self.max) return error.Full;
        const dst = self.buf[self.len..][0..rec];
        dst[0..abi.fuse_direntplus.NAME_OFFSET].* = abi.encode(abi.fuse_direntplus{
            .entry_out = entry.encode(),
            .dirent = .{ .ino = entry.attr.ino, .off = next_off, .namelen = @intCast(name.len), .type = direntType(entry.attr.mode) },
        });
        @memcpy(dst[abi.fuse_direntplus.NAME_OFFSET..][0..name.len], name);
        @memset(dst[abi.fuse_direntplus.NAME_OFFSET + name.len ..], 0);
        self.len += rec;
    }

    pub fn bytes(self: *const DirentPlusWriter) []const u8 {
        return self.buf[0..self.len];
    }
};

// ---------------------------------------------------------------------------
// INIT negotiation (fuse_abi_session.md §2.3–2.5, §2.8; charter §4)
// ---------------------------------------------------------------------------

/// What the filesystem asks of the negotiation; everything else is echoed
/// iff the kernel offered it, or never.
pub const InitWants = struct {
    /// Honest only with a REAL generation (derived from the volume-set identity).
    export_support: bool = false,
    dont_mask: bool = false,
    writeback_cache: bool = false,
    no_open_support: bool = false,
    no_opendir_support: bool = false,
    /// v1; never rides along with v2 unless asked.
    handle_killpriv: bool = false,
    /// The daemon's posture: suid always, sgid iff group-exec, caps drop.
    handle_killpriv_v2: bool = true,
    /// Answer plain READDIR with ENOSYS so every listing is READDIRPLUS.
    force_readdirplus: bool = false,
};

/// Echoed iff offered, unconditionally.
const echo_if_offered: u64 = abi.FUSE_ASYNC_READ | abi.FUSE_FILE_OPS | abi.FUSE_ATOMIC_O_TRUNC | abi.FUSE_BIG_WRITES |
    abi.FUSE_HAS_IOCTL_DIR | abi.FUSE_AUTO_INVAL_DATA | abi.FUSE_ASYNC_DIO | abi.FUSE_PARALLEL_DIROPS |
    abi.FUSE_MAX_PAGES | abi.FUSE_CACHE_SYMLINKS | abi.FUSE_DO_READDIRPLUS;

/// Never in a reply (charter §4; fuse_abi_session.md §2.5): advisory locks
/// stay kernel-local, splice is not a path, POSIX ACLs would probe every
/// create, the rest are capabilities this daemon has not implemented.
pub const never_advertised: u64 = abi.FUSE_POSIX_LOCKS | abi.FUSE_FLOCK_LOCKS | abi.FUSE_SPLICE_WRITE | abi.FUSE_SPLICE_MOVE |
    abi.FUSE_SPLICE_READ | abi.FUSE_POSIX_ACL | abi.FUSE_ABORT_ERROR | abi.FUSE_EXPLICIT_INVAL_DATA | abi.FUSE_MAP_ALIGNMENT |
    abi.FUSE_SETXATTR_EXT | abi.FUSE_INIT_RESERVED | abi.FUSE_SECURITY_CTX | abi.FUSE_HAS_INODE_DAX | abi.FUSE_CREATE_SUPP_GROUP |
    abi.FUSE_HAS_EXPIRE_ONLY | abi.FUSE_DIRECT_IO_ALLOW_MMAP | abi.FUSE_PASSTHROUGH | abi.FUSE_NO_EXPORT_SUPPORT |
    abi.FUSE_HAS_RESEND | abi.FUSE_ALLOW_IDMAP | abi.FUSE_REQUEST_TIMEOUT;

pub const InitReply = struct {
    out: abi.fuse_init_out,
    /// The folded capability word the kernel will APPLY: the classical reply
    /// bits, plus `flags2` iff `FUSE_INIT_EXT` rode (without it the kernel
    /// discards `flags2`).
    negotiated: u64,

    pub fn has(self: InitReply, bit: u64) bool {
        return self.negotiated & bit != 0;
    }

    pub fn timeLimits(self: InitReply) bool {
        return self.has(abi.FUSE_TIME_LIMITS);
    }

    pub fn minor(self: InitReply) u32 {
        return self.out.minor;
    }
};

pub const InitError = error{
    /// `major != 7` or a minor below the 64-byte `fuse_init_out` (< 7.23):
    /// outside the product's kernel floor (≥ 6.14); the session replies
    /// -EINVAL and fails loud.
    UnsupportedProtocol,
};

/// Oldest minor whose INIT reply is the 64-byte struct.
pub const MIN_SUPPORTED_MINOR: u32 = 23;

/// THE negotiation, pure: `fuse_init_in` + the resolved geometry + the
/// filesystem's wants → the 64-byte reply and the negotiated word.
pub fn buildInitReply(in: abi.fuse_init_in, p: geometry.Plan, wants: InitWants) InitError!InitReply {
    if (in.major != abi.FUSE_KERNEL_VERSION or in.minor < MIN_SUPPORTED_MINOR) return error.UnsupportedProtocol;
    const offered = in.folded();

    var want: u64 = echo_if_offered;
    if (wants.export_support) want |= abi.FUSE_EXPORT_SUPPORT;
    if (wants.dont_mask) want |= abi.FUSE_DONT_MASK;
    if (wants.writeback_cache) want |= abi.FUSE_WRITEBACK_CACHE;
    if (wants.no_open_support) want |= abi.FUSE_NO_OPEN_SUPPORT;
    if (wants.no_opendir_support) want |= abi.FUSE_NO_OPENDIR_SUPPORT;
    if (wants.handle_killpriv) want |= abi.FUSE_HANDLE_KILLPRIV;
    if (wants.handle_killpriv_v2) want |= abi.FUSE_HANDLE_KILLPRIV_V2;
    // READDIRPLUS_AUTO lets the kernel pick plain READDIR for `ls`-class
    // listings; forcing plus means never offering it the choice.
    if (!wants.force_readdirplus) want |= abi.FUSE_READDIRPLUS_AUTO;
    var reply_flags: u64 = offered & want;
    std.debug.assert(reply_flags & never_advertised == 0);

    // flags2: the ring transport always (no opt-out: the mount fails if it
    // cannot arm); the time range iff the kernel can apply it.
    var flags2: u64 = abi.FUSE_OVER_IO_URING;
    const time_limits = offered & abi.FUSE_TIME_LIMITS != 0;
    if (time_limits) flags2 |= abi.FUSE_TIME_LIMITS;

    const reply_minor = @min(in.minor, abi.FUSE_KERNEL_MINOR_VERSION);
    const init_ext = offered & abi.FUSE_INIT_EXT != 0 and flags2 != 0 and reply_minor >= 36;
    if (init_ext) reply_flags |= abi.FUSE_INIT_EXT;

    const out: abi.fuse_init_out = .{
        .major = abi.FUSE_KERNEL_VERSION,
        .minor = reply_minor,
        .max_readahead = in.max_readahead,
        .flags = abi.flagsOf(reply_flags),
        .max_background = p.max_background,
        .congestion_threshold = p.congestion_threshold,
        .max_write = p.max_write,
        .time_gran = abi.DEFAULT_TIME_GRAN,
        .max_pages = p.max_pages,
        .map_alignment = abi.DEFAULT_MAP_ALIGNMENT,
        .flags2 = abi.flags2Of(flags2),
        .max_stack_depth = 0,
        .request_timeout = 0,
        .unused = @splat(0),
        .time_min = if (time_limits) abi.TIME_LIMITS_MIN_SEC else 0,
        .time_max = if (time_limits) abi.TIME_LIMITS_MAX_SEC else 0,
    };
    return .{ .out = out, .negotiated = reply_flags | (if (init_ext) flags2 else 0) };
}

/// `len = 16 + 64`.
pub const INIT_REPLY_LEN: usize = OUT_HEADER_LEN + @sizeOf(abi.fuse_init_out);

pub fn replyInit(buf: *OutBuf, unique: u64, r: InitReply) Reply {
    return replyOut(buf, unique, r.out);
}

// ---------------------------------------------------------------------------
// Notify frames (fuse_abi_session.md §1.6, §8; Q13 fixed): `fuse_out_header
// {len = EVERY byte emitted, error = code, unique = 0} ‖ struct ‖ tail`.
// ---------------------------------------------------------------------------

pub const NotifyFrame = struct {
    /// `header ‖ struct ‖ inline tail` in the caller's buffer.
    head: []const u8,
    /// A zero-copy data view (STORE), else empty. `header.len == head.len + body.len`.
    body: []const u8,

    pub fn header(self: NotifyFrame) abi.fuse_out_header {
        return abi.decodePrefix(abi.fuse_out_header, self.head).?;
    }

    pub fn totalLen(self: NotifyFrame) usize {
        return self.head.len + self.body.len;
    }
};

pub const NotifyError = error{
    BufferTooSmall,
    NameTooLong,
};

/// Bytes a name-carrying frame needs at most: header + the largest struct +
/// `FUSE_NAME_MAX` + NUL.
pub const NOTIFY_NAME_FRAME_MAX: usize = OUT_HEADER_LEN + @sizeOf(abi.fuse_notify_delete_out) + abi.FUSE_NAME_MAX + 1;

/// THE frame builder: `len` is computed from what is emitted (`head` +
/// `body_len`), so a frame can never claim a length the write does not carry.
fn notifyFrame(buf: []u8, code: abi.NotifyCode, out: anytype, tail: []const u8, body_len: usize) NotifyError!NotifyFrame {
    const n = @sizeOf(@TypeOf(out));
    const head_len = OUT_HEADER_LEN + n + tail.len;
    if (buf.len < head_len) return error.BufferTooSmall;
    const total = head_len + body_len;
    buf[0..OUT_HEADER_LEN].* = abi.encode(abi.fuse_out_header{ .len = @intCast(total), .@"error" = @intCast(@backingInt(code)), .unique = 0 });
    buf[OUT_HEADER_LEN..][0..n].* = abi.encode(out);
    @memcpy(buf[OUT_HEADER_LEN + n ..][0..tail.len], tail);
    return .{ .head = buf[0..head_len], .body = &.{} };
}

/// A `name ‖ NUL` tail staged after the struct in `buf` (the kernel requires
/// `size == sizeof(out) + namelen + 1`).
fn nameTail(buf: []u8, comptime Out: type, name: []const u8) NotifyError![]const u8 {
    if (name.len > abi.FUSE_NAME_MAX) return error.NameTooLong;
    const at = OUT_HEADER_LEN + @sizeOf(Out);
    if (buf.len < at + name.len + 1) return error.BufferTooSmall;
    const tail = buf[at..][0 .. name.len + 1];
    @memcpy(tail[0..name.len], name);
    tail[name.len] = 0;
    return tail;
}

/// `off < 0` = attributes only; `(0, 0)` = every page to EOF.
pub fn notifyInvalInode(buf: []u8, ino: u64, off: i64, len: i64) NotifyError!NotifyFrame {
    return notifyFrame(buf, .inval_inode, abi.fuse_notify_inval_inode_out{ .ino = ino, .off = off, .len = len }, &.{}, 0);
}

/// `flags`: 0 = invalidate, `FUSE_EXPIRE_ONLY` = expire (uapi ≥ 7.38).
pub fn notifyInvalEntry(buf: []u8, parent: u64, name: []const u8, flags: u32) NotifyError!NotifyFrame {
    const tail = try nameTail(buf, abi.fuse_notify_inval_entry_out, name);
    return notifyFrameInPlace(buf, .inval_entry, abi.fuse_notify_inval_entry_out{ .parent = parent, .namelen = @intCast(name.len), .flags = flags }, tail.len);
}

pub fn notifyDelete(buf: []u8, parent: u64, child: u64, name: []const u8) NotifyError!NotifyFrame {
    const tail = try nameTail(buf, abi.fuse_notify_delete_out, name);
    return notifyFrameInPlace(buf, .delete, abi.fuse_notify_delete_out{ .parent = parent, .child = child, .namelen = @intCast(name.len), .padding = 0 }, tail.len);
}

/// The tail is already staged in `buf` after the struct slot: write header
/// and struct around it.
fn notifyFrameInPlace(buf: []u8, code: abi.NotifyCode, out: anytype, tail_len: usize) NotifyError!NotifyFrame {
    const n = @sizeOf(@TypeOf(out));
    const head_len = OUT_HEADER_LEN + n + tail_len;
    std.debug.assert(buf.len >= head_len);
    buf[0..OUT_HEADER_LEN].* = abi.encode(abi.fuse_out_header{ .len = @intCast(head_len), .@"error" = @intCast(@backingInt(code)), .unique = 0 });
    buf[OUT_HEADER_LEN..][0..n].* = abi.encode(out);
    return .{ .head = buf[0..head_len], .body = &.{} };
}

/// STORE: the data is a zero-copy body view; `len` covers it.
pub fn notifyStore(buf: []u8, nodeid: u64, offset: u64, data: []const u8) NotifyError!NotifyFrame {
    var f = try notifyFrame(buf, .store, abi.fuse_notify_store_out{ .nodeid = nodeid, .offset = offset, .size = @intCast(data.len), .padding = 0 }, &.{}, data.len);
    f.body = data;
    return f;
}

pub fn notifyRetrieve(buf: []u8, notify_unique: u64, nodeid: u64, offset: u64, size: u32) NotifyError!NotifyFrame {
    return notifyFrame(buf, .retrieve, abi.fuse_notify_retrieve_out{ .notify_unique = notify_unique, .nodeid = nodeid, .offset = offset, .size = size, .padding = 0 }, &.{}, 0);
}

/// PRUNE (uapi 7.45): the nodeid array rides inline; `len` MUST cover it.
pub fn notifyPrune(buf: []u8, nodeids: []const u64) NotifyError!NotifyFrame {
    const at = OUT_HEADER_LEN + @sizeOf(abi.fuse_notify_prune_out);
    const tail_len = nodeids.len * @sizeOf(u64);
    if (buf.len < at + tail_len) return error.BufferTooSmall;
    for (nodeids, 0..) |id, i| std.mem.writeInt(u64, buf[at + i * 8 ..][0..8], id, .little);
    return notifyFrameInPlace(buf, .prune, abi.fuse_notify_prune_out{ .count = @intCast(nodeids.len), .padding = 0, .spare = 0 }, tail_len);
}

pub fn notifyPollWakeup(buf: []u8, kh: u64) NotifyError!NotifyFrame {
    return notifyFrame(buf, .poll, abi.fuse_notify_poll_wakeup_out{ .kh = kh }, &.{}, 0);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testHeader(op: abi.Opcode, unique: u64, nodeid: u64, body_len: usize) abi.fuse_in_header {
    return .{ .len = @intCast(HEADER_LEN + body_len), .opcode = @backingInt(op), .unique = unique, .nodeid = nodeid, .uid = 1000, .gid = 1000, .pid = 4242, .total_extlen = 0, .padding = 0 };
}

/// A classical device read: `header ‖ parts…` into `buf`; returns `n`.
fn classical(buf: []u8, op: abi.Opcode, parts: []const []const u8) usize {
    var n: usize = 0;
    for (parts) |p| n += p.len;
    buf[0..HEADER_LEN].* = abi.encode(testHeader(op, 7, 1, n));
    var at: usize = HEADER_LEN;
    for (parts) |p| {
        @memcpy(buf[at..][0..p.len], p);
        at += p.len;
    }
    return at;
}

test "body bounds (FUSE-3g): a short header never underflows; an overdeclared body is refused, not clamped over stale bytes; the lease body rides the payload; the buffer capacity is the last bound" {
    try testing.expectEqual(BodyBounds.short_header, validatedBody(0, 100, 0, 8192));
    try testing.expectEqual(BodyBounds.short_header, validatedBody(39, 100, 0, 8192));
    try testing.expectEqual(BodyBounds{ .valid = 0 }, validatedBody(40, 100, 0, 8192));
    try testing.expectEqual(BodyBounds{ .valid = 60 }, validatedBody(100, 60, 0, 8192));
    // Declared 100 but only 60 arrived: the other 40 would be the previous request's.
    try testing.expectEqual(BodyBounds{ .overdeclared = .{ .declared = 100, .available = 60 } }, validatedBody(140, 60, 0, 8192));
    // A WRITE lease: 40 bytes of struct in the buffer, 4096 in the leased payload.
    try testing.expectEqual(BodyBounds{ .valid = 40 }, validatedBody(40 + 40 + 4096, 40, 4096, 8192));
    try testing.expectEqual(BodyBounds{ .overdeclared = .{ .declared = 4137, .available = 4136 } }, validatedBody(40 + 40 + 4097, 40, 4096, 8192));
    // The buffer's capacity bounds the slice even when more was "filled".
    try testing.expectEqual(BodyBounds{ .valid = 100 }, validatedBody(240, 200, 0, 100));
    try testing.expectEqual(BodyBounds{ .valid = 0 }, validatedBody(std.math.maxInt(u32), std.math.maxInt(usize), 0, 0));
}

test "classical delivery: n < 40 is a short read (nothing to address); a lying len refuses; the body splits at the opcode's in_args[0].size" {
    var buf: [8192]u8 = undefined;
    try testing.expectError(error.ShortRead, Delivery.fromClassical(&buf, 39));
    try testing.expectError(error.ShortRead, Delivery.fromClassical(buf[0..10], 10));
    const before = requests_malformed.load();
    var h = testHeader(.lookup, 1, 1, 500);
    buf[0..HEADER_LEN].* = abi.encode(h);
    try testing.expectError(error.Overdeclared, Delivery.fromClassical(&buf, HEADER_LEN + 6));
    h.len = 10;
    buf[0..HEADER_LEN].* = abi.encode(h);
    try testing.expectError(error.ShortHeader, Delivery.fromClassical(&buf, HEADER_LEN + 6));
    try testing.expectEqual(before + 2, requests_malformed.load());
    // MKDIR: struct (8) then the name.
    const n = classical(&buf, .mkdir, &.{ &abi.encode(abi.fuse_mkdir_in{ .mode = 0o755, .umask = 0o22 }), "dir\x00" });
    const d = try Delivery.fromClassical(&buf, n);
    try testing.expectEqual(@as(usize, 8), d.op_in.len);
    try testing.expectEqualStrings("dir\x00", d.payload);
    // LOOKUP: zero arg0 — the name IS the payload.
    const n2 = classical(&buf, .lookup, &.{"file\x00"});
    const d2 = try Delivery.fromClassical(&buf, n2);
    try testing.expectEqual(@as(usize, 0), d2.op_in.len);
    try testing.expectEqualStrings("file\x00", d2.payload);
}

test "ring delivery: the triple maps straight onto the Delivery — a name-only op has an empty op_in and its name in the payload; WRITE data is the leased payload itself (zero-copy); stale op_in bytes past in_args[0] are never exposed" {
    var in_out: [128]u8 = @splat(0xEE);
    var op_in: [128]u8 = @splat(0xEE);
    in_out[0..HEADER_LEN].* = abi.encode(testHeader(.lookup, 9, 1, 5));
    const name = "hello\x00";
    const d = try Delivery.fromRing(&in_out, &op_in, name, name.len);
    try testing.expectEqual(@as(usize, 0), d.op_in.len);
    try testing.expectEqual(@as(u64, 9), d.header.unique);
    const r = try parse(d);
    try testing.expectEqualStrings("hello", r.lookup.name);
    try testing.expectEqual(name.ptr, r.lookup.name.ptr);

    const win: abi.fuse_write_in = .{ .fh = 3, .offset = 4096, .size = 4096, .write_flags = abi.FUSE_WRITE_KILL_SUIDGID, .lock_owner = 0, .flags = 0, .padding = 0 };
    in_out[0..HEADER_LEN].* = abi.encode(testHeader(.write, 11, 1, 40 + 4096));
    op_in[0..40].* = abi.encode(win);
    var payload: [8192]u8 = @splat(0xAB);
    const dw = try Delivery.fromRing(&in_out, &op_in, payload[0..4096], 4096);
    try testing.expectEqual(@as(usize, 40), dw.op_in.len);
    const rw = try parse(dw);
    try testing.expectEqual(@as(u64, 4096), rw.write.in.offset);
    try testing.expectEqual(@as(usize, 4096), rw.write.data.len);
    try testing.expectEqual(@as([*]const u8, &payload), rw.write.data.ptr);
    try testing.expect(rw.write.in.write_flags & abi.FUSE_WRITE_KILL_SUIDGID != 0);
    // A held zero-copy slot: empty placeholder, authoritative length from
    // the slot; the parse admits it with an empty data view of size 4096.
    const dh = try Delivery.fromRing(&in_out, &op_in, &.{}, 4096);
    try testing.expectEqual(@as(usize, 0), dh.payload.len);
    try testing.expectEqual(@as(usize, 4096), dh.held_len);
    const rh = try parse(dh);
    try testing.expectEqual(@as(u32, 4096), rh.write.in.size);
    try testing.expectEqual(@as(usize, 0), rh.write.data.len);
    // A placeholder whose length disagrees with the struct is still a short payload.
    in_out[0..HEADER_LEN].* = abi.encode(testHeader(.write, 11, 1, 40 + 2048));
    var op_short = op_in;
    op_short[0..40].* = abi.encode(abi.fuse_write_in{ .fh = 3, .offset = 0, .size = 4096, .write_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 });
    try testing.expectError(error.ShortPayload, parse(try Delivery.fromRing(&in_out, &op_short, &.{}, 2048)));
    in_out[0..HEADER_LEN].* = abi.encode(testHeader(.write, 11, 1, 40 + 4096));
    // Overdeclared against op_in + payload refuses.
    in_out[0..HEADER_LEN].* = abi.encode(testHeader(.write, 11, 1, 40 + 8192));
    try testing.expectError(error.Overdeclared, Delivery.fromRing(&in_out, &op_in, payload[0..4096], 4096));
}

test "parse: every op's names and sizes — two-name ops, xattr value by size, forget arrays by count, INIT prefix with flags2 law, flag-derived fields" {
    var buf: [8192]u8 = undefined;
    // RENAME: struct + two names.
    var n = classical(&buf, .rename, &.{ &abi.encode(abi.fuse_rename_in{ .newdir = 77 }), "old\x00", "new\x00" });
    var r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u64, 77), r.rename.newdir);
    try testing.expectEqualStrings("old", r.rename.name);
    try testing.expectEqualStrings("new", r.rename.newname);
    try testing.expectEqual(@as(u32, 0), r.rename.flags);
    // RENAME2 carries flags.
    n = classical(&buf, .rename2, &.{ &abi.encode(abi.fuse_rename2_in{ .newdir = 78, .flags = 1, .padding = 0 }), "a\x00", "b\x00" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u32, 1), r.rename2.flags);
    try testing.expectEqualStrings("b", r.rename2.newname);
    // SYMLINK: zero arg0, name then link.
    n = classical(&buf, .symlink, &.{ "lnk\x00", "/target/path\x00" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqualStrings("lnk", r.symlink.name);
    try testing.expectEqualStrings("/target/path", r.symlink.link);
    // SETXATTR: name then exactly `size` bytes of value.
    n = classical(&buf, .setxattr, &.{ &abi.encode(abi.fuse_setxattr_in{ .size = 3, .flags = 1 }), "user.k\x00", "val", "junk" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqualStrings("user.k", r.setxattr.name);
    try testing.expectEqualStrings("val", r.setxattr.value);
    try testing.expectEqual(@as(u32, 1), r.setxattr.flags);
    n = classical(&buf, .setxattr, &.{ &abi.encode(abi.fuse_setxattr_in{ .size = 30, .flags = 0 }), "user.k\x00", "val" });
    try testing.expectError(error.ShortPayload, parse(try Delivery.fromClassical(&buf, n)));
    // GETXATTR / LISTXATTR / REMOVEXATTR.
    n = classical(&buf, .getxattr, &.{ &abi.encode(abi.fuse_getxattr_in{ .size = 0, .padding = 0 }), "user.a\x00" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqualStrings("user.a", r.getxattr.name);
    try testing.expectEqual(@as(u32, 0), r.getxattr.size);
    n = classical(&buf, .listxattr, &.{&abi.encode(abi.fuse_getxattr_in{ .size = 512, .padding = 0 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u32, 512), r.listxattr.size);
    n = classical(&buf, .removexattr, &.{"user.z\x00"});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqualStrings("user.z", r.removexattr.name);
    // BATCH_FORGET: count × 16; a count past the payload refuses (never an allocation authority).
    n = classical(&buf, .batch_forget, &.{ &abi.encode(abi.fuse_batch_forget_in{ .count = 2, .dummy = 0 }), &abi.encode(abi.fuse_forget_one{ .nodeid = 5, .nlookup = 2 }), &abi.encode(abi.fuse_forget_one{ .nodeid = 6, .nlookup = 1 }) });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(usize, 2), r.batch_forget.entries.len);
    try testing.expectEqual(@as(u64, 6), r.batch_forget.entries[1].nodeid);
    try testing.expectEqual(@as(u64, 2), r.batch_forget.entries[0].nlookup);
    n = classical(&buf, .batch_forget, &.{ &abi.encode(abi.fuse_batch_forget_in{ .count = 1_000_000, .dummy = 0 }), &abi.encode(abi.fuse_forget_one{ .nodeid = 5, .nlookup = 2 }) });
    try testing.expectError(error.ShortPayload, parse(try Delivery.fromClassical(&buf, n)));
    // FORGET.
    n = classical(&buf, .forget, &.{&abi.encode(abi.fuse_forget_in{ .nlookup = 9 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u64, 9), r.forget.nlookup);
    // NOTIFY_REPLY: zero arg0 — the retrieve struct leads the payload, then `size` bytes.
    n = classical(&buf, .notify_reply, &.{ &abi.encode(abi.fuse_notify_retrieve_in{ .dummy1 = 0, .offset = 8192, .size = 4, .dummy2 = 0, .dummy3 = 0, .dummy4 = 0 }), "DATA" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u64, 8192), r.notify_reply.offset);
    try testing.expectEqualStrings("DATA", r.notify_reply.data);
    n = classical(&buf, .notify_reply, &.{ &abi.encode(abi.fuse_notify_retrieve_in{ .dummy1 = 0, .offset = 0, .size = 40, .dummy2 = 0, .dummy3 = 0, .dummy4 = 0 }), "DATA" });
    try testing.expectError(error.ShortPayload, parse(try Delivery.fromClassical(&buf, n)));
    // GETATTR: fh present iff FUSE_GETATTR_FH.
    n = classical(&buf, .getattr, &.{&abi.encode(abi.fuse_getattr_in{ .getattr_flags = abi.FUSE_GETATTR_FH, .dummy = 0, .fh = 44 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(?u64, 44), r.getattr.fh);
    n = classical(&buf, .getattr, &.{&abi.encode(abi.fuse_getattr_in{ .getattr_flags = 0, .dummy = 0, .fh = 44 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(?u64, null), r.getattr.fh);
    // FSYNC datasync bit; RELEASE flush bit stays on the struct.
    n = classical(&buf, .fsync, &.{&abi.encode(abi.fuse_fsync_in{ .fh = 1, .fsync_flags = 1, .padding = 0 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expect(r.fsync.datasync);
    n = classical(&buf, .release, &.{&abi.encode(abi.fuse_release_in{ .fh = 1, .flags = 0, .release_flags = abi.FUSE_RELEASE_FLUSH, .lock_owner = 5 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expect(r.release.release_flags & abi.FUSE_RELEASE_FLUSH != 0);
    // CREATE / MKNOD / LINK / OPEN / ACCESS / LSEEK / BMAP / FLUSH / INTERRUPT.
    n = classical(&buf, .create, &.{ &abi.encode(abi.fuse_create_in{ .flags = 0o1101, .mode = 0o100644, .umask = 0o22, .open_flags = abi.FUSE_OPEN_KILL_SUIDGID }), "f\x00" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqualStrings("f", r.create.name);
    try testing.expectEqual(@as(u32, 0o100644), r.create.mode);
    try testing.expectEqual(abi.FUSE_OPEN_KILL_SUIDGID, r.create.open_flags);
    n = classical(&buf, .mknod, &.{ &abi.encode(abi.fuse_mknod_in{ .mode = 0o20644, .rdev = 0x0105, .umask = 0, .padding = 0 }), "dev\x00" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u32, 0x0105), r.mknod.rdev);
    n = classical(&buf, .link, &.{ &abi.encode(abi.fuse_link_in{ .oldnodeid = 12 }), "hard\x00" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u64, 12), r.link.oldnodeid);
    try testing.expectEqualStrings("hard", r.link.newname);
    n = classical(&buf, .open, &.{&abi.encode(abi.fuse_open_in{ .flags = 2, .open_flags = 1 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u32, 1), r.open.open_flags);
    n = classical(&buf, .access, &.{&abi.encode(abi.fuse_access_in{ .mask = 4, .padding = 0 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u32, 4), r.access.mask);
    n = classical(&buf, .lseek, &.{&abi.encode(abi.fuse_lseek_in{ .fh = 1, .offset = 10, .whence = 3, .padding = 0 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u32, 3), r.lseek.whence);
    n = classical(&buf, .bmap, &.{&abi.encode(abi.fuse_bmap_in{ .block = 8, .blocksize = 4096, .padding = 0 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u64, 8), r.bmap.block);
    n = classical(&buf, .flush, &.{&abi.encode(abi.fuse_flush_in{ .fh = 2, .unused = 0, .padding = 0, .lock_owner = 99 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u64, 99), r.flush.lock_owner);
    n = classical(&buf, .interrupt, &.{&abi.encode(abi.fuse_interrupt_in{ .unique = 1234 })});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u64, 1234), r.interrupt.unique);
    // IOCTL: struct then `in_size` bytes.
    n = classical(&buf, .ioctl, &.{ &abi.encode(abi.fuse_ioctl_in{ .fh = 1, .flags = 0, .cmd = 0x1234, .arg = 0, .in_size = 2, .out_size = 0 }), "io" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqualStrings("io", r.ioctl.data);
    // No-arg ops.
    n = classical(&buf, .readlink, &.{});
    try testing.expect((try parse(try Delivery.fromClassical(&buf, n))) == .readlink);
    n = classical(&buf, .statfs, &.{});
    try testing.expect((try parse(try Delivery.fromClassical(&buf, n))) == .statfs);
    n = classical(&buf, .destroy, &.{});
    try testing.expect((try parse(try Delivery.fromClassical(&buf, n))) == .destroy);
    // Unknown and not-served opcodes decode as `unsupported` (reply -ENOSYS).
    n = classical(&buf, .statx, &.{});
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(abi.Opcode.statx, r.unsupported);
    n = classical(&buf, abi.Opcode.fromInt(99), &.{ "any", "thing" });
    r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(u32, 99), @backingInt(r.unsupported));
}

test "parse refusals: a missing NUL, a short struct, a total_extlen we never negotiated — each -EINVAL and counted" {
    var buf: [8192]u8 = undefined;
    const before = requests_malformed.load();
    var n = classical(&buf, .lookup, &.{"no-nul"});
    try testing.expectError(error.MissingNul, parse(try Delivery.fromClassical(&buf, n)));
    n = classical(&buf, .mkdir, &.{ &[_]u8{ 1, 2, 3 }, "x\x00" });
    try testing.expectError(error.ShortOpIn, parse(try Delivery.fromClassical(&buf, n)));
    n = classical(&buf, .init, &.{&@as([12]u8, @splat(0))});
    try testing.expectError(error.ShortOpIn, parse(try Delivery.fromClassical(&buf, n)));
    n = classical(&buf, .rename, &.{ &abi.encode(abi.fuse_rename_in{ .newdir = 1 }), "only-one\x00" });
    try testing.expectError(error.MissingNul, parse(try Delivery.fromClassical(&buf, n)));
    var h = testHeader(.statfs, 1, 1, 0);
    h.total_extlen = 8;
    buf[0..HEADER_LEN].* = abi.encode(h);
    try testing.expectError(error.UnexpectedExtension, parse(try Delivery.fromClassical(&buf, HEADER_LEN)));
    try testing.expectEqual(before + 5, requests_malformed.load());
    try testing.expectEqual(linux.E.INVAL, parseErrno(error.MissingNul));
    try testing.expectEqual(linux.E.INVAL, parseErrno(error.UnexpectedExtension));
}

test "INIT decode: the 16-byte prefix suffices; flags2 is read iff INIT_EXT ∧ len ≥ 20, else 0; the 64-byte form decodes whole; < 16 is refused" {
    const classic_full = abi.encode(abi.fuse_init_in{ .major = 7, .minor = 31, .max_readahead = 131072, .flags = 0x7ff, .flags2 = 0xFFFF, .unused = @splat(0) });
    const classic16 = classic_full[0..16];
    const a = decodeInitIn(classic16).?;
    try testing.expectEqual(@as(u32, 31), a.minor);
    try testing.expectEqual(@as(u32, 0), a.flags2);
    // INIT_EXT set but only 16 bytes: no flags2 word to read.
    const ext_full = abi.encode(abi.fuse_init_in{ .major = 7, .minor = 44, .max_readahead = 0, .flags = abi.flagsOf(abi.FUSE_INIT_EXT), .flags2 = 0x200, .unused = @splat(0) });
    try testing.expectEqual(@as(u32, 0), decodeInitIn(ext_full[0..16]).?.flags2);
    // INIT_EXT with ≥ 20 bytes: flags2 valid.
    try testing.expectEqual(@as(u32, 0x200), decodeInitIn(ext_full[0..20]).?.flags2);
    // No INIT_EXT with 64 bytes: flags2 discarded.
    const noext64 = abi.encode(abi.fuse_init_in{ .major = 7, .minor = 44, .max_readahead = 0, .flags = 1, .flags2 = 0x200, .unused = @splat(0) });
    try testing.expectEqual(@as(u32, 0), decodeInitIn(&noext64).?.flags2);
    const full = abi.encode(abi.fuse_init_in{ .major = 7, .minor = 44, .max_readahead = 0, .flags = abi.flagsOf(abi.FUSE_INIT_EXT), .flags2 = 0x200, .unused = @splat(7) });
    const f = decodeInitIn(&full).?;
    try testing.expectEqual(@as(u32, 0x200), f.flags2);
    try testing.expectEqual(@as(u32, 7), f.unused[10]);
    try testing.expect(decodeInitIn(full[0..15]) == null);
    try testing.expect(decodeInitIn(&.{}) == null);
    // Through the request parser: raw_len is what the kernel sent.
    var buf: [8192]u8 = undefined;
    const n = classical(&buf, .init, &.{classic16});
    const r = try parse(try Delivery.fromClassical(&buf, n));
    try testing.expectEqual(@as(usize, 16), r.init.raw_len);
    try testing.expectEqual(@as(u32, 131072), r.init.in.max_readahead);
}

test "error header: always NEGATIVE errno through the one table (Q6); len 16; ENOSYS for the unserved; typed errors map" {
    const h = errorHeader(42, .INVAL);
    try testing.expectEqual(@as(u32, 16), h.len);
    try testing.expectEqual(@as(i32, -22), h.@"error");
    try testing.expectEqual(@as(u64, 42), h.unique);
    try testing.expectEqual(@as(i32, -2), replyError(1, .NOENT).header.@"error");
    try testing.expectEqual(@as(i32, -38), replyEnosys(1).header.@"error");
    try testing.expectEqual(@as(i32, -11), replyErrorFor(1, error.QueueFull).header.@"error");
    try testing.expectEqual(@as(i32, -5), replyErrorFor(1, error.RingFailed).header.@"error");
    try testing.expectEqual(@as(i32, -30), replyErrorFor(1, error.DismountRefused).header.@"error");
    try testing.expect(replyError(1, .IO).isError());
    try testing.expectEqual(@as(usize, 0), replyError(1, .IO).extraLen());
    const ok = replyOk(3);
    try testing.expect(!ok.isError());
    try testing.expectEqual(@as(u32, 16), ok.header.len);
    // The bytes on the wire.
    const b = abi.encode(errorHeader(0x0102, .NOENT));
    try testing.expectEqualSlices(u8, &.{ 16, 0, 0, 0, 0xFE, 0xFF, 0xFF, 0xFF, 0x02, 0x01, 0, 0, 0, 0, 0, 0 }, &b);
}

test "TTL split is nanosecond-exact (Q2): 1.5 s → (1, 500_000_000); 0.25 s → (0, 250_000_000); attr stamps floor below the epoch" {
    try testing.expectEqual(SplitTime{ .sec = 1, .nsec = 500_000_000 }, splitTtlNs(1_500_000_000));
    try testing.expectEqual(SplitTime{ .sec = 0, .nsec = 250_000_000 }, splitTtlNs(250_000_000));
    try testing.expectEqual(SplitTime{ .sec = 0, .nsec = 0 }, splitTtlNs(0));
    try testing.expectEqual(SplitTime{ .sec = 1, .nsec = 1 }, splitTtlNs(1_000_000_001));
    try testing.expectEqual(SplitTime{ .sec = 1_700_000_000, .nsec = 123_456_789 }, splitTimeNs(1_700_000_000_123_456_789));
    const neg = splitTimeNs(-1);
    try testing.expectEqual(@as(i64, -1), @as(i64, @bitCast(neg.sec)));
    try testing.expectEqual(@as(u32, 999_999_999), neg.nsec);
    const min = splitTimeNs(std.math.minInt(i64));
    try testing.expect(min.nsec < 1_000_000_000);
    try testing.expectEqual(@as(i64, -9_223_372_037), @as(i64, @bitCast(min.sec)));
    try testing.expectEqual(@as(u32, 145_224_192), min.nsec);
}

fn sampleAttr(ino: u64) abi.fuse_attr {
    var a = abi.zeroed(abi.fuse_attr);
    a.ino = ino;
    a.mode = 0o100644;
    a.nlink = 1;
    a.size = 4096;
    return a;
}

test "entry reply (Q1): SEPARATE entry and attr TTLs; CREATE and LOOKUP encode the same entry bytes (Q2 tie); a negative entry is nodeid 0 + entry TTL with zero attrs; the negative counter moves" {
    var buf: OutBuf = undefined;
    const e: EntryReply = .{ .nodeid = 10, .generation = 3, .entry_ttl_ns = 1_500_000_000, .attr_ttl_ns = 250_000_000, .attr = sampleAttr(10) };
    const r = replyEntry(&buf, 5, e);
    try testing.expectEqual(@as(u32, 16 + 128), r.header.len);
    try testing.expectEqual(@as(usize, 128), r.out.len);
    const out = abi.decodeExact(abi.fuse_entry_out, r.out).?;
    try testing.expectEqual(@as(u64, 1), out.entry_valid);
    try testing.expectEqual(@as(u32, 500_000_000), out.entry_valid_nsec);
    try testing.expectEqual(@as(u64, 0), out.attr_valid);
    try testing.expectEqual(@as(u32, 250_000_000), out.attr_valid_nsec);
    try testing.expectEqual(@as(u64, 3), out.generation);
    try testing.expectEqual(@as(u64, 10), out.attr.ino);
    // CREATE = the same entry bytes ‖ open_out.
    var buf2: OutBuf = undefined;
    const c = replyCreate(&buf2, 5, e, 0xFEED, abi.FOPEN_KEEP_CACHE | abi.FOPEN_NOFLUSH | abi.FOPEN_PARALLEL_DIRECT_WRITES);
    try testing.expectEqual(@as(u32, 16 + 144), c.header.len);
    try testing.expectEqualSlices(u8, r.out, c.out[0..128]);
    const o = abi.decodeExact(abi.fuse_open_out, c.out[128..144]).?;
    try testing.expectEqual(@as(u64, 0xFEED), o.fh);
    try testing.expectEqual(@as(u32, 0b1100010), o.open_flags);
    try testing.expectEqual(@as(i32, 0), o.backing_id);
    // Negative.
    const before = lookup_negative_replies.load();
    const neg = replyEntry(&buf, 6, EntryReply.negative(1_000_000_000));
    const n = abi.decodeExact(abi.fuse_entry_out, neg.out).?;
    try testing.expectEqual(@as(u64, 0), n.nodeid);
    try testing.expectEqual(@as(u64, 1), n.entry_valid);
    try testing.expectEqual(@as(u32, 0), n.entry_valid_nsec);
    try testing.expectEqual(@as(u64, 0), n.attr_valid);
    try testing.expectEqual(abi.zeroed(abi.fuse_attr), n.attr);
    try testing.expect(!neg.isError());
    try testing.expectEqual(before + 1, lookup_negative_replies.load());
    try testing.expectEqual(before + 1, blk: {
        _ = replyEntry(&buf, 7, e);
        break :blk lookup_negative_replies.load();
    });
}

test "attr/open/write/statfs/xattr/lseek/bmap/poll/lk/ioctl replies: sizes and fields; dummy and padding stay 0" {
    var buf: OutBuf = undefined;
    const a = replyAttr(&buf, 1, .{ .attr_ttl_ns = 2_000_000_001, .attr = sampleAttr(4) });
    try testing.expectEqual(@as(u32, 16 + 104), a.header.len);
    const ao = abi.decodeExact(abi.fuse_attr_out, a.out).?;
    try testing.expectEqual(@as(u64, 2), ao.attr_valid);
    try testing.expectEqual(@as(u32, 1), ao.attr_valid_nsec);
    try testing.expectEqual(@as(u32, 0), ao.dummy);
    try testing.expectEqual(@as(u64, 4), ao.attr.ino);
    const o = replyOpen(&buf, 2, 77, abi.FOPEN_DIRECT_IO);
    try testing.expectEqual(@as(u32, 32), o.header.len);
    try testing.expectEqual(@as(u64, 77), abi.decodeExact(abi.fuse_open_out, o.out).?.fh);
    const w = replyWrite(&buf, 3, 4096);
    try testing.expectEqual(@as(u32, 24), w.header.len);
    try testing.expectEqual(@as(u32, 4096), abi.decodeExact(abi.fuse_write_out, w.out).?.size);
    const s = replyStatfs(&buf, 4, .{ .blocks = 100, .bfree = 50, .bavail = 40, .files = 10, .ffree = 5, .bsize = 4096, .namelen = 255, .frsize = 4096 });
    try testing.expectEqual(@as(u32, 96), s.header.len);
    const st = abi.decodeExact(abi.fuse_statfs_out, s.out).?.st;
    try testing.expectEqual(@as(u64, 40), st.bavail);
    try testing.expectEqual(@as(u32, 255), st.namelen);
    try testing.expectEqual(@as(u32, 0), st.padding);
    try testing.expectEqual(@as([6]u32, @splat(0)), st.spare);
    const x = replyXattrSize(&buf, 5, 17);
    try testing.expectEqual(@as(u32, 24), x.header.len);
    try testing.expect(!x.isError());
    try testing.expectEqual(@as(u32, 17), abi.decodeExact(abi.fuse_getxattr_out, x.out).?.size);
    const l = replyLseek(&buf, 6, 1 << 40);
    try testing.expectEqual(@as(u32, 24), l.header.len);
    try testing.expectEqual(@as(u64, 1 << 40), abi.decodeExact(abi.fuse_lseek_out, l.out).?.offset);
    const b = replyBmap(&buf, 7, 9);
    try testing.expectEqual(@as(u64, 9), abi.decodeExact(abi.fuse_bmap_out, b.out).?.block);
    const p = replyPoll(&buf, 8, 5);
    try testing.expectEqual(@as(u32, 5), abi.decodeExact(abi.fuse_poll_out, p.out).?.revents);
    const lk = replyLk(&buf, 9, .{ .start = 0, .end = 10, .type = 1, .pid = 2 });
    try testing.expectEqual(@as(u32, 40), lk.header.len);
    const io = replyIoctl(&buf, 10, .{ .result = 0, .flags = 0, .in_iovs = 0, .out_iovs = 0 }, "abc");
    try testing.expectEqual(@as(u32, 16 + 16 + 3), io.header.len);
    try testing.expectEqualStrings("abc", io.body);
}

test "data replies are zero-copy views; READ truncates to size; the oversize rule names the EIO fallback" {
    const data = "hello, world";
    const r = replyData(1, data);
    try testing.expectEqual(@as(u32, 16 + 12), r.header.len);
    try testing.expectEqual(@as([*]const u8, data.ptr), r.body.ptr);
    try testing.expectEqual(@as(usize, 0), r.out.len);
    const t = replyRead(2, data, 5);
    try testing.expectEqual(@as(u32, 21), t.header.len);
    try testing.expectEqualStrings("hello", t.body);
    try testing.expectEqual(@as([*]const u8, data.ptr), t.body.ptr);
    try testing.expectEqualStrings(data, replyRead(2, data, 1000).body);
    // Oversize: the caller's verdict, then a header-only -EIO.
    try testing.expect(!r.oversize(12));
    try testing.expect(r.oversize(11));
    var buf: OutBuf = undefined;
    try testing.expect(replyEntry(&buf, 1, EntryReply.negative(0)).oversize(127));
    try testing.expect(!replyEntry(&buf, 1, EntryReply.negative(0)).oversize(128));
    try testing.expect(!replyOk(1).oversize(0));
    const eio = replyError(r.header.unique, .IO);
    try testing.expectEqual(@as(i32, -5), eio.header.@"error");
    try testing.expectEqual(@as(u32, 16), eio.header.len);
}

test "readZeroTail: a short paged READ zero-completes the requested pages past the served bytes (the copy path's page_zeroing), bounded by the window; a full reply and a reply past the request's last page touch nothing" {
    var win: [3 * 4096]u8 = @splat(0xAA);
    // 1,959 of 8,192 requested: [1959, 8192) zero, the third page untouched.
    try testing.expectEqual(@as(u32, 8192), readZeroTail(&win, 1959, 8192, 4096));
    try testing.expect(std.mem.allEqual(u8, win[0..1959], 0xAA));
    try testing.expect(std.mem.allEqual(u8, win[1959..8192], 0));
    try testing.expect(std.mem.allEqual(u8, win[8192..], 0xAA));
    // An unaligned request size rounds up to its last page.
    win = @splat(0xAA);
    try testing.expectEqual(@as(u32, 8192), readZeroTail(&win, 100, 4097, 4096));
    try testing.expect(std.mem.allEqual(u8, win[100..8192], 0));
    try testing.expect(std.mem.allEqual(u8, win[8192..], 0xAA));
    // Full reply: nothing zeroed. A request wider than the window is bounded.
    win = @splat(0xAA);
    try testing.expectEqual(@as(u32, 8192), readZeroTail(&win, 8192, 8192, 4096));
    try testing.expect(std.mem.allEqual(u8, &win, 0xAA));
    try testing.expectEqual(@as(u32, win.len), readZeroTail(&win, 12000, 1 << 20, 4096));
    try testing.expect(std.mem.allEqual(u8, win[0..12000], 0xAA));
    try testing.expect(std.mem.allEqual(u8, win[12000..], 0));
}

test "attrFromInode: i64-ns stamps split exact, rdev is the kernel's 32-bit encoding verbatim, blocks are 512-byte units, flags 0" {
    const v: record.InodeValue = .{ .mode = 0o100755, .uid = 1, .gid = 2, .nlink = 3, .flags = 0, .rdev = 0x0105, .size = 1025, .atime = @bitCast(@as(i64, 1_700_000_000_000_000_001)), .mtime = @bitCast(@as(i64, -1_500_000_000)), .ctime = 0, .flags2 = 0 };
    const a = attrFromInode(99, v, 4096);
    try testing.expectEqual(@as(u64, 99), a.ino);
    try testing.expectEqual(@as(u64, 1025), a.size);
    try testing.expectEqual(@as(u64, 3), a.blocks);
    try testing.expectEqual(@as(u64, 1_700_000_000), a.atime);
    try testing.expectEqual(@as(u32, 1), a.atimensec);
    try testing.expectEqual(@as(i64, -2), @as(i64, @bitCast(a.mtime)));
    try testing.expectEqual(@as(u32, 500_000_000), a.mtimensec);
    try testing.expectEqual(@as(u64, 0), a.ctime);
    try testing.expectEqual(@as(u32, 0o100755), a.mode);
    try testing.expectEqual(@as(u32, 3), a.nlink);
    try testing.expectEqual(@as(u32, 0x0105), a.rdev);
    try testing.expectEqual(@as(u32, 4096), a.blksize);
    try testing.expectEqual(@as(u32, 0), a.flags);
    try testing.expectEqual(@as(u32, 0o10), direntType(0o100644));
    try testing.expectEqual(@as(u32, 0o4), direntType(0o40755));
    try testing.expectEqual(@as(u32, 0o12), direntType(0o120777));
}

test "dirent packing: 8-byte aligned, zero-padded, type = mode >> 12, the cookie is the NEXT offset, stop when the next record would exceed size; empty/long names refused" {
    var body: [256]u8 = @splat(0xCC);
    var w = DirentWriter.init(&body, 80);
    try w.add(1, 1, 0o40755, ".");
    try testing.expectEqual(@as(usize, 32), w.len);
    const d0 = abi.decodePrefix(abi.fuse_dirent, w.bytes()).?;
    try testing.expectEqual(@as(u64, 1), d0.ino);
    try testing.expectEqual(@as(u64, 1), d0.off);
    try testing.expectEqual(@as(u32, 1), d0.namelen);
    try testing.expectEqual(@as(u32, 4), d0.type);
    try testing.expectEqual(@as(u8, '.'), body[24]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0 }, body[25..32]);
    try w.add(7, 2, 0o100644, "file.txt");
    try testing.expectEqual(@as(usize, 64), w.len);
    try testing.expectEqual(@as(u32, 8), abi.decodePrefix(abi.fuse_dirent, w.bytes()[32..]).?.type);
    // 64 + 32 > 80: stop; what was packed so far is the reply.
    try testing.expectError(error.Full, w.add(8, 3, 0o100644, "x"));
    try testing.expectEqual(@as(usize, 64), w.bytes().len);
    try testing.expectError(error.EmptyName, w.add(8, 3, 0o100644, ""));
    const long: [abi.FUSE_NAME_MAX + 1]u8 = @splat('a');
    try testing.expectError(error.NameTooLong, w.add(8, 3, 0o100644, &long));
    const r = replyData(1, w.bytes());
    try testing.expectEqual(@as(u32, 80), r.header.len);
    // The buffer bounds the size too.
    var small: [40]u8 = undefined;
    var ws = DirentWriter.init(&small, 4096);
    try ws.add(1, 1, 0, "a");
    try testing.expectError(error.Full, ws.add(2, 2, 0, "b"));
}

test "direntplus packing: 152 + name padded to 8; the entry carries both TTL classes through the ONE encoder; a nodeid-0 entry is legal" {
    var body: [512]u8 = undefined;
    var w = DirentPlusWriter.init(&body, 480);
    const e: EntryReply = .{ .nodeid = 10, .generation = 0, .entry_ttl_ns = 1_000_000_000, .attr_ttl_ns = 500_000_000, .attr = sampleAttr(10) };
    try w.add(e, 1, "abc");
    try testing.expectEqual(@as(usize, 160), w.len);
    const rec = abi.decodePrefix(abi.fuse_direntplus, w.bytes()).?;
    try testing.expectEqual(e.encode(), rec.entry_out);
    try testing.expectEqual(@as(u64, 10), rec.dirent.ino);
    try testing.expectEqual(@as(u64, 1), rec.dirent.off);
    try testing.expectEqual(@as(u32, 3), rec.dirent.namelen);
    try testing.expectEqual(@as(u32, 8), rec.dirent.type);
    try testing.expectEqualStrings("abc", body[152..155]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0 }, body[155..160]);
    var dot = EntryReply.negative(0);
    dot.attr.mode = 0o40755;
    try w.add(dot, 2, ".");
    try testing.expectEqual(@as(usize, 320), w.len);
    // 320 + (152 + 18 → 176) = 496 > 480: stop.
    try testing.expectError(error.Full, w.add(e, 3, "overflow-name-here"));
}

fn initIn(minor: u32, offered: u64) abi.fuse_init_in {
    return .{ .major = 7, .minor = minor, .max_readahead = 131072, .flags = abi.flagsOf(offered), .flags2 = abi.flags2Of(offered), .unused = @splat(0) };
}

const test_plan: geometry.Plan = .{ .nqueues = 32, .depth = 32, .max_write = 1 << 20, .max_pages = 256, .payload_sz = 1 << 20, .max_background = 1024, .congestion_threshold = 768, .page = 4096, .max_pages_limit = 256 };

/// Every classical bit a 6.14-class kernel offers plus the two flags2 bits.
const kernel_6_14_offer: u64 = (@as(u64, 1) << 31) - 1 | abi.FUSE_INIT_EXT | abi.FUSE_OVER_IO_URING | abi.FUSE_HAS_EXPIRE_ONLY | abi.FUSE_HAS_RESEND;

test "INIT reply: always OVER_IO_URING; INIT_EXT iff offered; minor = min(kernel, 36); max_readahead verbatim; geometry fields; time_gran 1; len 80" {
    var buf: OutBuf = undefined;
    const r = try buildInitReply(initIn(44, kernel_6_14_offer), test_plan, .{});
    try testing.expectEqual(@as(u32, 7), r.out.major);
    try testing.expectEqual(@as(u32, 36), r.out.minor);
    try testing.expectEqual(@as(u32, 131072), r.out.max_readahead);
    try testing.expect(r.has(abi.FUSE_OVER_IO_URING));
    try testing.expect(r.has(abi.FUSE_INIT_EXT));
    try testing.expectEqual(abi.flags2Of(abi.FUSE_OVER_IO_URING), r.out.flags2);
    try testing.expectEqual(@as(u16, 1024), r.out.max_background);
    try testing.expectEqual(@as(u16, 768), r.out.congestion_threshold);
    try testing.expectEqual(@as(u32, 1 << 20), r.out.max_write);
    try testing.expectEqual(@as(u16, 256), r.out.max_pages);
    try testing.expectEqual(@as(u32, 1), r.out.time_gran);
    try testing.expectEqual(@as(u16, 0), r.out.map_alignment);
    try testing.expectEqual(@as(u32, 0), r.out.max_stack_depth);
    try testing.expectEqual(@as(u16, 0), r.out.request_timeout);
    try testing.expectEqual([_]u16{ 0, 0, 0 }, r.out.unused);
    const rep = replyInit(&buf, 2, r);
    try testing.expectEqual(@as(u32, INIT_REPLY_LEN), rep.header.len);
    try testing.expectEqual(@as(u32, 80), rep.header.len);
    try testing.expectEqual(@as(usize, 64), rep.out.len);
    // A 7.31 kernel: minor 31, no INIT_EXT (not offered), flags2 still written but NOT negotiated.
    const old = try buildInitReply(initIn(31, (@as(u64, 1) << 29) - 1), test_plan, .{});
    try testing.expectEqual(@as(u32, 31), old.out.minor);
    try testing.expect(!old.has(abi.FUSE_INIT_EXT));
    try testing.expect(!old.has(abi.FUSE_OVER_IO_URING));
    try testing.expectEqual(abi.flags2Of(abi.FUSE_OVER_IO_URING), old.out.flags2);
    // INIT_EXT is never invented without the kernel's offer, even on a new minor.
    const noext = try buildInitReply(initIn(44, kernel_6_14_offer & ~abi.FUSE_INIT_EXT), test_plan, .{});
    try testing.expect(!noext.has(abi.FUSE_INIT_EXT));
    try testing.expect(noext.out.flags & abi.flagsOf(abi.FUSE_INIT_EXT) == 0);
}

test "INIT reply never advertises POSIX_LOCKS, FLOCK_LOCKS, SPLICE_*, POSIX_ACL or any unimplemented bit — over the whole offer space" {
    var prng = std.Random.DefaultPrng.init(0x1217);
    const rnd = prng.random();
    for (0..2000) |_| {
        const offered = rnd.int(u64);
        const r = try buildInitReply(initIn(rnd.intRangeAtMost(u32, 23, 60), offered), test_plan, .{ .export_support = rnd.boolean(), .writeback_cache = rnd.boolean(), .dont_mask = rnd.boolean(), .handle_killpriv = rnd.boolean(), .handle_killpriv_v2 = rnd.boolean(), .force_readdirplus = rnd.boolean() });
        try testing.expectEqual(@as(u64, 0), r.negotiated & never_advertised);
        try testing.expectEqual(@as(u64, 0), r.out.folded() & never_advertised);
        // Every classical reply bit except INIT_EXT was offered.
        try testing.expectEqual(@as(u64, 0), (@as(u64, r.out.flags) & ~abi.FUSE_INIT_EXT) & ~offered);
        // flags2 carries only the ring bit and (iff offered) the time bit.
        try testing.expectEqual(@as(u64, 0), (@as(u64, r.out.flags2) << 32) & ~(abi.FUSE_OVER_IO_URING | abi.FUSE_TIME_LIMITS));
        try testing.expect(r.out.minor <= 36);
    }
    const all = try buildInitReply(initIn(44, std.math.maxInt(u64)), test_plan, .{});
    try testing.expect(!all.has(abi.FUSE_POSIX_LOCKS));
    try testing.expect(!all.has(abi.FUSE_FLOCK_LOCKS));
    try testing.expect(!all.has(abi.FUSE_SPLICE_WRITE | abi.FUSE_SPLICE_MOVE | abi.FUSE_SPLICE_READ));
    try testing.expect(!all.has(abi.FUSE_POSIX_ACL));
    try testing.expect(all.has(abi.FUSE_ASYNC_READ));
    try testing.expect(all.has(abi.FUSE_MAX_PAGES));
    try testing.expect(all.has(abi.FUSE_CACHE_SYMLINKS));
    try testing.expect(all.has(abi.FUSE_DO_READDIRPLUS));
    try testing.expect(all.has(abi.FUSE_READDIRPLUS_AUTO));
    try testing.expect(all.has(abi.FUSE_PARALLEL_DIROPS));
}

test "INIT reply echoes the time range iff FUSE_TIME_LIMITS (bit 62) was offered: ±9_223_372_036 s and flags2 bit 30, else zeros" {
    const with = try buildInitReply(initIn(44, kernel_6_14_offer | abi.FUSE_TIME_LIMITS), test_plan, .{});
    try testing.expect(with.timeLimits());
    try testing.expectEqual(abi.TIME_LIMITS_MIN_SEC, with.out.time_min);
    try testing.expectEqual(abi.TIME_LIMITS_MAX_SEC, with.out.time_max);
    try testing.expectEqual(abi.flags2Of(abi.FUSE_OVER_IO_URING | abi.FUSE_TIME_LIMITS), with.out.flags2);
    try testing.expectEqual(@as(u32, (1 << 9) | (1 << 30)), with.out.flags2);
    const b = abi.encode(with.out);
    try testing.expectEqual(abi.TIME_LIMITS_MIN_SEC, std.mem.readInt(i64, b[48..56], .little));
    try testing.expectEqual(abi.TIME_LIMITS_MAX_SEC, std.mem.readInt(i64, b[56..64], .little));
    const without = try buildInitReply(initIn(44, kernel_6_14_offer), test_plan, .{});
    try testing.expect(!without.timeLimits());
    try testing.expectEqual(@as(i64, 0), without.out.time_min);
    try testing.expectEqual(@as(i64, 0), without.out.time_max);
    // Offered but without INIT_EXT the kernel cannot apply flags2: not negotiated.
    const stranded = try buildInitReply(initIn(44, (kernel_6_14_offer | abi.FUSE_TIME_LIMITS) & ~abi.FUSE_INIT_EXT), test_plan, .{});
    try testing.expect(!stranded.timeLimits());
}

test "INIT reply option gates: KILLPRIV_V2 iff offered ∧ wanted (v1 never rides along unless asked); writeback/dont_mask/no_open/no_opendir/export iff wanted; force_readdirplus drops READDIRPLUS_AUTO" {
    const on = try buildInitReply(initIn(44, kernel_6_14_offer), test_plan, .{ .handle_killpriv_v2 = true });
    try testing.expect(on.has(abi.FUSE_HANDLE_KILLPRIV_V2));
    try testing.expect(!on.has(abi.FUSE_HANDLE_KILLPRIV));
    const off = try buildInitReply(initIn(44, kernel_6_14_offer), test_plan, .{ .handle_killpriv_v2 = false });
    try testing.expect(!off.has(abi.FUSE_HANDLE_KILLPRIV_V2));
    const unoffered = try buildInitReply(initIn(44, kernel_6_14_offer & ~abi.FUSE_HANDLE_KILLPRIV_V2), test_plan, .{ .handle_killpriv_v2 = true });
    try testing.expect(!unoffered.has(abi.FUSE_HANDLE_KILLPRIV_V2));
    const v1 = try buildInitReply(initIn(44, kernel_6_14_offer), test_plan, .{ .handle_killpriv = true, .handle_killpriv_v2 = false });
    try testing.expect(v1.has(abi.FUSE_HANDLE_KILLPRIV));
    const plain = try buildInitReply(initIn(44, kernel_6_14_offer), test_plan, .{});
    try testing.expect(!plain.has(abi.FUSE_WRITEBACK_CACHE));
    try testing.expect(!plain.has(abi.FUSE_DONT_MASK));
    try testing.expect(!plain.has(abi.FUSE_NO_OPEN_SUPPORT));
    try testing.expect(!plain.has(abi.FUSE_NO_OPENDIR_SUPPORT));
    try testing.expect(!plain.has(abi.FUSE_EXPORT_SUPPORT));
    try testing.expect(plain.has(abi.FUSE_READDIRPLUS_AUTO));
    const opts = try buildInitReply(initIn(44, kernel_6_14_offer), test_plan, .{ .writeback_cache = true, .dont_mask = true, .no_open_support = true, .no_opendir_support = true, .export_support = true, .force_readdirplus = true });
    try testing.expect(opts.has(abi.FUSE_WRITEBACK_CACHE));
    try testing.expect(opts.has(abi.FUSE_DONT_MASK));
    try testing.expect(opts.has(abi.FUSE_NO_OPEN_SUPPORT));
    try testing.expect(opts.has(abi.FUSE_NO_OPENDIR_SUPPORT));
    try testing.expect(opts.has(abi.FUSE_EXPORT_SUPPORT));
    try testing.expect(opts.has(abi.FUSE_DO_READDIRPLUS));
    try testing.expect(!opts.has(abi.FUSE_READDIRPLUS_AUTO));
}

test "INIT reply minor law over the whole shape space: min(kernel, 36); INIT_EXT needs offered ∧ minor ≥ 36; an unknown major or a pre-7.23 minor refuses" {
    for (23..80) |m| {
        const minor: u32 = @intCast(m);
        const r = try buildInitReply(initIn(minor, kernel_6_14_offer), test_plan, .{});
        try testing.expectEqual(@min(minor, 36), r.out.minor);
        try testing.expectEqual(minor >= 36, r.has(abi.FUSE_INIT_EXT));
        try testing.expectEqual(minor >= 36, r.has(abi.FUSE_OVER_IO_URING));
    }
    try testing.expectError(error.UnsupportedProtocol, buildInitReply(.{ .major = 8, .minor = 0, .max_readahead = 0, .flags = 0, .flags2 = 0, .unused = @splat(0) }, test_plan, .{}));
    try testing.expectError(error.UnsupportedProtocol, buildInitReply(.{ .major = 6, .minor = 50, .max_readahead = 0, .flags = 0, .flags2 = 0, .unused = @splat(0) }, test_plan, .{}));
    try testing.expectError(error.UnsupportedProtocol, buildInitReply(initIn(22, 0), test_plan, .{}));
    // The INIT decode-failure reply is a NEGATIVE EINVAL (Q6).
    try testing.expectEqual(@as(i32, -22), replyError(2, .INVAL).header.@"error");
}

fn expectNotifyLaw(f: NotifyFrame, code: abi.NotifyCode, struct_len: usize, tail_len: usize) !void {
    const h = f.header();
    try testing.expectEqual(@as(u64, 0), h.unique);
    try testing.expectEqual(@as(i32, @intCast(@backingInt(code))), h.@"error");
    try testing.expectEqual(@as(u32, @intCast(16 + struct_len + tail_len)), h.len);
    // THE law: len == every byte the write carries.
    try testing.expectEqual(@as(usize, h.len), f.totalLen());
}

test "notify frames (Q13): len covers struct AND tail, names carry the NUL, unique 0, error = code — every code against the uapi length law" {
    var buf: [NOTIFY_NAME_FRAME_MAX]u8 = undefined;
    const ii = try notifyInvalInode(&buf, 42, -1, 0);
    try expectNotifyLaw(ii, .inval_inode, 24, 0);
    const iio = abi.decodeExact(abi.fuse_notify_inval_inode_out, ii.head[16..]).?;
    try testing.expectEqual(@as(u64, 42), iio.ino);
    try testing.expectEqual(@as(i64, -1), iio.off);
    try testing.expectEqual(@as(usize, 40), ii.totalLen());

    const ie = try notifyInvalEntry(&buf, 1, "name", abi.FUSE_EXPIRE_ONLY);
    try expectNotifyLaw(ie, .inval_entry, 16, 5);
    const ieo = abi.decodePrefix(abi.fuse_notify_inval_entry_out, ie.head[16..]).?;
    try testing.expectEqual(@as(u32, 4), ieo.namelen);
    try testing.expectEqual(abi.FUSE_EXPIRE_ONLY, ieo.flags);
    try testing.expectEqualSlices(u8, "name\x00", ie.head[32..37]);

    const de = try notifyDelete(&buf, 1, 9, "gone");
    try expectNotifyLaw(de, .delete, 24, 5);
    const deo = abi.decodePrefix(abi.fuse_notify_delete_out, de.head[16..]).?;
    try testing.expectEqual(@as(u64, 9), deo.child);
    try testing.expectEqual(@as(u32, 4), deo.namelen);
    try testing.expectEqualSlices(u8, "gone\x00", de.head[40..45]);

    const data = "page-bytes";
    const st = try notifyStore(&buf, 5, 4096, data);
    try expectNotifyLaw(st, .store, 24, data.len);
    try testing.expectEqual(@as(usize, 40), st.head.len);
    try testing.expectEqual(@as([*]const u8, data.ptr), st.body.ptr);
    try testing.expectEqual(@as(u32, 10), abi.decodeExact(abi.fuse_notify_store_out, st.head[16..]).?.size);

    const re = try notifyRetrieve(&buf, 77, 5, 0, 8192);
    try expectNotifyLaw(re, .retrieve, 32, 0);
    try testing.expectEqual(@as(u64, 77), abi.decodeExact(abi.fuse_notify_retrieve_out, re.head[16..]).?.notify_unique);

    const pr = try notifyPrune(&buf, &.{ 10, 20, 30 });
    try expectNotifyLaw(pr, .prune, 16, 24);
    const pro = abi.decodePrefix(abi.fuse_notify_prune_out, pr.head[16..]).?;
    try testing.expectEqual(@as(u32, 3), pro.count);
    try testing.expectEqual(@as(u64, 20), std.mem.readInt(u64, pr.head[40..48], .little));
    const pr0 = try notifyPrune(&buf, &.{});
    try expectNotifyLaw(pr0, .prune, 16, 0);

    const pw = try notifyPollWakeup(&buf, 3);
    try expectNotifyLaw(pw, .poll, 8, 0);

    // Refusals: a short buffer, an over-long name.
    var tiny: [20]u8 = undefined;
    try testing.expectError(error.BufferTooSmall, notifyInvalInode(&tiny, 1, 0, 0));
    try testing.expectError(error.BufferTooSmall, notifyPrune(&tiny, &.{1}));
    try testing.expectError(error.BufferTooSmall, notifyInvalEntry(&tiny, 1, "n", 0));
    const long: [abi.FUSE_NAME_MAX + 1]u8 = @splat('x');
    try testing.expectError(error.NameTooLong, notifyInvalEntry(&buf, 1, &long, 0));
    try testing.expectError(error.NameTooLong, notifyDelete(&buf, 1, 2, &long));
    const max_name: [abi.FUSE_NAME_MAX]u8 = @splat('y');
    const ok = try notifyDelete(&buf, 1, 2, &max_name);
    try expectNotifyLaw(ok, .delete, 24, abi.FUSE_NAME_MAX + 1);
}

/// The whole request path on arbitrary bytes: never a trap.
fn checkRequestTotal(buf: []const u8, n: usize) void {
    const d = Delivery.fromClassical(buf, n) catch return;
    _ = parse(d) catch return;
}

fn checkRingTotal(in_out: *const [128]u8, op_in: *const [128]u8, payload: []const u8) void {
    const d = Delivery.fromRing(in_out, op_in, payload, payload.len) catch return;
    _ = parse(d) catch return;
}

test "totality: classical and ring parsing over random headers (every opcode 0..64) and bodies, deterministic" {
    var prng = std.Random.DefaultPrng.init(0xC0DEC);
    const r = prng.random();
    var buf: [4096]u8 = undefined;
    var in_out: [128]u8 = undefined;
    var op_in: [128]u8 = undefined;
    for (0..20_000) |_| {
        r.bytes(&buf);
        var h = testHeader(abi.Opcode.fromInt(r.intRangeAtMost(u32, 0, 64)), r.int(u64), r.int(u64), 0);
        h.len = if (r.boolean()) r.intRangeAtMost(u32, 0, 300) else r.int(u32);
        h.total_extlen = if (r.intRangeAtMost(u8, 0, 15) == 0) r.int(u16) else 0;
        buf[0..HEADER_LEN].* = abi.encode(h);
        checkRequestTotal(&buf, r.intRangeAtMost(usize, 0, buf.len));
        r.bytes(&in_out);
        r.bytes(&op_in);
        in_out[0..HEADER_LEN].* = abi.encode(h);
        checkRingTotal(&in_out, &op_in, buf[0..r.intRangeAtMost(usize, 0, buf.len)]);
    }
    _ = decodeInitIn(buf[0..r.intRangeAtMost(usize, 0, 100)]);
}

fn requestFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [2048]u8 = undefined;
    const len = smith.slice(&buf);
    checkRequestTotal(buf[0..len], len);
    if (len >= 128) {
        var op_in: [128]u8 = undefined;
        smith.bytes(&op_in);
        checkRingTotal(buf[0..128], &op_in, buf[128..len]);
    }
    _ = decodeInitIn(buf[0..len]);
}

test "fuzz: the request path is total on arbitrary bytes" {
    try fuzzkit.fuzz({}, requestFuzz, .{});
}
