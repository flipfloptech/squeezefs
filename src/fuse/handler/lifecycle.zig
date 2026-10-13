//! The request lifecycle on a handler lane (`docs/porting/
//! fuse_handlers_lanes.md` §1, §3, §8; charter §4 "for every request
//! delivered on a slot exactly one COMMIT … is submitted"; §9).
//!
//! `Dispatcher(Transport).dispatch` IS the ordered procedure the transport
//! hands a decoded request to on the lane homed on the requester's CPU
//! (every opcode — the reference rotated non-READ opcodes across all lanes,
//! ledger L2):
//!
//!   classify the op (its watchdog kind + phase class)
//!   → mint the ONE reply obligation for the slot
//!   → prologue: `fuse_ops`, the watchdog slot claim, `handler_entry`
//!   → the level-1 op guard as the op's class demands (WRITE: the §2.4
//!     admission; SETATTR-size: exclusive, the whole truncate; rung 3's
//!     inline READ takes none — one KV snapshot read, nothing to guard)
//!   → the filesystem op (typed args in, typed reply or an error out)
//!   → the accounting that must precede the reply (open count, lookup
//!     references: a RELEASE the kernel sends after our OPEN reply must
//!     find the count already there)
//!   → release the guard at the class's drop point
//!   → encode (the one errno table for an error) → release the WRITE lease
//!     → `handler_return` → the ONE commit by slot ref → `reply_commit`
//!   → epilogue: registry release, the trace's `finish`.
//!
//! Every failure arm of the spec's table is explicit code here: a handler
//! error replies its errno; a parse refusal arrived as EINVAL before us;
//! an unknown or unserved opcode is ENOSYS (the kernel latches `fc->
//! no_<op>`); a duplicate INIT is a protocol violation (EINVAL, warned); a
//! reply the window cannot hold is a header-only EIO (`fuse_replies_
//! oversize`, must-stay-0); a handler that returned WITHOUT a reply having
//! been committed is caught by the obligation's `deinit` (header-only EIO,
//! `fuse_replies_synthesized`, must-stay-0 — Zig has no unwinding, so this
//! class is a logic error, never a panic's residue); a second reply on one
//! obligation is dropped and counted (`fuse_reply_twice`, must-stay-0) —
//! never an overwrite. A handler that panics aborts the process loud
//! (ReleaseSafe; decision 0002 rows 9/23): the transport's shutdown
//! synthesizer covers the slots a dead lane owed.
//!
//! The no-reply opcodes never mint an obligation: FORGET / BATCH_FORGET
//! return lookup references through the open table and call the
//! filesystem's `forget` once per ino whose last reference went (§8; the
//! transport auto-committed their ents at delivery); INTERRUPT is decoded,
//! counted (`fuse_interrupts_received`) and IGNORED — nothing is
//! interruptible (a handler parked mid-commit is never dropped: charter §4;
//! ledger L3), the kernel's own `fuse_request_end` on the signal already
//! returns EINTR to the application, and our later reply lands on a
//! forgotten unique (benign); NOTIFY_REPLY answers a RETRIEVE this daemon
//! never sends.

const std = @import("std");
const Io = std.Io;
const linux = std.os.linux;
const assert = std.debug.assert;
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const transport_slot = @import("../transport/slot.zig");
const seam = @import("transport_seam.zig");
const filesystem = @import("filesystem.zig");
const op_guard = @import("op_guard.zig");
const op_trace = @import("op_trace.zig");
const watchdog = @import("watchdog.zig");
const open_table = @import("open_table.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const errors = @import("../../core/errors.zig");

pub const Filesystem = filesystem.Filesystem;
pub const Ctx = filesystem.Ctx;
pub const Error = filesystem.Error;

/// The file-size ceiling every WRITE range is validated against: `u32::MAX`
/// block indices × 4 MiB blocks = 16 PiB (charter §5.6 "EFBIG at
/// u32::MAX"). A rung-3 inline file refuses far below it (`kv_fs`).
pub const MAX_FILE_SIZE: u64 = @as(u64, std.math.maxInt(u32)) * (4 << 20);

// ---------------------------------------------------------------------------
// Instruments (§10).
// ---------------------------------------------------------------------------

/// `fuse_ops`: every reply-owing handler entry.
pub var fuse_ops: counters.ShardedCounter = .{};
/// `fuse_interrupts_received`: INTERRUPTs decoded and ignored.
pub var fuse_interrupts_received: counters.ShardedCounter = .{};
/// `fuse_forgets`: FORGET + BATCH_FORGET entries accounted.
pub var fuse_forgets: counters.ShardedCounter = .{};
/// `fuse_forget_evictions`: entries whose last kernel reference went.
pub var fuse_forget_evictions: counters.ShardedCounter = .{};
/// `fuse_release_clean_fastpath` / `fuse_flush_clean_fastpath`: never-
/// dirtied handles elided (FLUSH answers ENOSYS — the kernel's `no_flush`
/// latch; RELEASE skips the flush arm).
pub var fuse_release_clean_fastpath: counters.ShardedCounter = .{};
pub var fuse_flush_clean_fastpath: counters.ShardedCounter = .{};
/// `fuse_unsupported_opcodes`: ENOSYS replies to opcodes this daemon does
/// not serve (the kernel latches and stops asking — a count that keeps
/// growing names a kernel that does not latch).
pub var fuse_unsupported_opcodes: counters.ShardedCounter = .{};
/// `fuse_duplicate_inits`: a second INIT on a live session (EINVAL).
pub var fuse_duplicate_inits: counters.ShardedCounter = .{};
/// A handler returned with its obligation still owed (must stay 0).
pub var fuse_replies_synthesized: counters.Tripwire = .init("fuse_replies_synthesized");
/// A second reply on one obligation (must stay 0).
pub var fuse_reply_twice: counters.Tripwire = .init("fuse_reply_twice");
/// A reply the window could not hold (must stay 0: handlers write into the
/// window, so the bound is structural).
pub var fuse_replies_oversize: counters.Tripwire = .init("fuse_replies_oversize");
/// A MOVE class reached the reply (decision 0003 §X.1: the move gate
/// retries every one across the slot move; one answered EAGAIN here is an
/// op path the gate does not cover). Must stay 0.
pub var fuse_move_class_at_boundary: counters.Tripwire = .init("fuse_move_class_at_boundary");

/// The request header's identity fields the dispatcher needs.
pub const Meta = struct {
    unique: u64,
    nodeid: u64,
    uid: u32,
    gid: u32,
    pid: u32,
};

pub const Options = struct {
    fs: Filesystem,
    io: Io,
    registry: *watchdog.Registry,
    guards: *op_guard.Table,
    opens: *open_table.OpenTable,
    /// The data plane's shared-mode probe (`Probe.none` at rung 3).
    probe: op_guard.Probe = op_guard.Probe.none,
    /// `SQUEEZEFS_WRITE_SHARED` as read at mount.
    shared_enabled: bool = true,
};

pub fn Dispatcher(comptime Transport: type) type {
    seam.assertTransport(Transport);
    return struct {
        const Self = @This();

        transport: *Transport,
        fs: Filesystem,
        io: Io,
        registry: *watchdog.Registry,
        guards: *op_guard.Table,
        opens: *open_table.OpenTable,
        probe: op_guard.Probe,
        shared_enabled: bool,

        /// FUSE-2: exactly one reply per delivered request. The value owes
        /// a reply from construction; `reply`/`fail` consume it; `deinit`
        /// synthesizes a header-only EIO if nothing did.
        pub const Obligation = struct {
            transport: *Transport,
            slot: Transport.SlotRef,
            unique: u64,
            owed: bool = true,

            pub fn reply(self: *Obligation, r: codec.Reply) void {
                if (!self.owed) {
                    fuse_reply_twice.fire("a second reply on one obligation was dropped");
                    return;
                }
                self.owed = false;
                self.transport.submitReply(self.slot, r);
            }

            pub fn fail(self: *Obligation, errno: linux.E) void {
                if (!self.owed) {
                    fuse_reply_twice.fire("a second (error) reply on one obligation was dropped");
                    return;
                }
                self.owed = false;
                self.transport.failEnt(self.slot, self.unique, errno);
            }

            pub fn deinit(self: *Obligation) void {
                if (!self.owed) return;
                fuse_replies_synthesized.fire("handler returned without replying; -EIO synthesized");
                self.owed = false;
                self.transport.failEnt(self.slot, self.unique, .IO);
            }
        };

        pub fn init(transport: *Transport, opts: Options) Self {
            return .{
                .transport = transport,
                .fs = opts.fs,
                .io = opts.io,
                .registry = opts.registry,
                .guards = opts.guards,
                .opens = opts.opens,
                .probe = opts.probe,
                .shared_enabled = opts.shared_enabled,
            };
        }

        /// THE procedure (module doc). `lease` is the WRITE payload lease
        /// (released before the reply) — null for every other opcode.
        pub fn dispatch(self: *Self, req: codec.Request, meta: Meta, slot: Transport.SlotRef, lease: ?Transport.PayloadLease) void {
            const op = opcodeOf(req);
            switch (req) {
                .forget => |f| {
                    fuse_forgets.inc();
                    self.forgetOne(meta, meta.nodeid, f.nlookup);
                    self.releaseLease(lease);
                    return;
                },
                .batch_forget => |b| {
                    for (b.entries) |e| {
                        fuse_forgets.inc();
                        self.forgetOne(meta, e.nodeid, e.nlookup);
                    }
                    self.releaseLease(lease);
                    return;
                },
                .interrupt => {
                    fuse_interrupts_received.inc();
                    self.releaseLease(lease);
                    return;
                },
                .notify_reply => {
                    self.releaseLease(lease);
                    return;
                },
                else => {},
            }

            var ob: Obligation = .{ .transport = self.transport, .slot = slot, .unique = meta.unique };
            defer ob.deinit();
            fuse_ops.inc();
            var trace = op_trace.OpTrace.begin(meta.unique, op_trace.OpClass.of(op));
            defer trace.finish();
            const wd = self.registry.claim(op, meta.nodeid, meta.unique);
            defer if (wd) |s| self.registry.release(s);
            var ctx: Ctx = .{
                .unique = meta.unique,
                .uid = meta.uid,
                .gid = meta.gid,
                .pid = meta.pid,
                .io = self.io,
                .trace = &trace,
                .wd_slot = wd,
                .window = self.transport.replyWindow(slot),
            };
            ctx.stage(.handler_entry);
            var out: codec.OutBuf = undefined;
            const outcome = self.run(req, meta, &ctx, &out, lease);
            // The payload lease ends BEFORE the commit: the transport's gate
            // would otherwise park the reply until it did.
            self.releaseLease(lease);
            ctx.stage(.handler_return);
            if (outcome) |reply| {
                if (reply.oversize(ctx.window.len)) {
                    fuse_replies_oversize.fire("reply larger than the ent payload window; header-only EIO committed");
                    ob.fail(.IO);
                } else {
                    ob.reply(reply);
                }
            } else |err| {
                // The error IS the reply: header-only `-errno` through the
                // ONE table, by the transport's one non-reply exit. A move
                // class here escaped the move gate (the tripwire); any
                // other retryable answer names its cause at debug level.
                if (errors.isMoveClass(err)) {
                    fuse_move_class_at_boundary.fire("a move class reached the FUSE reply (EAGAIN to the application)");
                    log.warn("handler: {t} on ino {d} (unique {d}) answered EAGAIN for {s} — an op path the move gate does not cover", .{ op, meta.nodeid, meta.unique, @errorName(err) });
                } else if (errors.classOf(err) == .retryable) log.debug("handler: {t} on ino {d} (unique {d}) answered EAGAIN: {s}", .{ op, meta.nodeid, meta.unique, @errorName(err) });
                ob.fail(errors.toErrno(err));
            }
            ctx.stage(.reply_commit);
        }

        /// The FUSE drain worker's try-only answer to a LOOKUP or GETATTR
        /// (decision 0003 §AB.1), on the worker thread: the filesystem's
        /// `probe_lookup` / `probe_getattr`, the lookup reference noted as
        /// the lane would; null = any refusal (the lane re-asks the
        /// blocking op and answers it, errors included). Counted in
        /// `fuse_ops` and the class's latency like a lane answer.
        pub fn probeMeta(self: *Self, req: codec.Request, meta: Meta, out: *codec.OutBuf) ?codec.Reply {
            const op = opcodeOf(req);
            var trace = op_trace.OpTrace.begin(meta.unique, op_trace.OpClass.of(op));
            var ctx: Ctx = .{ .unique = meta.unique, .uid = meta.uid, .gid = meta.gid, .pid = meta.pid, .io = self.io, .trace = &trace, .wd_slot = null, .window = &.{} };
            const reply: codec.Reply = switch (req) {
                .lookup => |a| blk: {
                    const e = self.fs.vtable.probe_lookup(self.fs.impl, &ctx, .{ .parent = meta.nodeid, .name = a.name }) catch return null;
                    self.noteEntry(e) catch return null;
                    break :blk codec.replyEntry(out, meta.unique, e);
                },
                .getattr => |a| codec.replyAttr(out, meta.unique, self.fs.vtable.probe_getattr(self.fs.impl, &ctx, .{ .ino = meta.nodeid, .fh = a.fh }) catch return null),
                else => return null,
            };
            fuse_ops.inc();
            trace.finish();
            return reply;
        }

        fn releaseLease(self: *Self, lease: ?Transport.PayloadLease) void {
            if (lease) |l| self.transport.releaseLease(l);
        }

        /// A ctx for the session-driven hooks (INIT negotiated, teardown,
        /// the worker-side FORGET hand-off): no reply window, no registry
        /// slot, the identity of the session itself.
        fn sessionCtx(self: *Self, trace: *op_trace.OpTrace) Ctx {
            return .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = self.io, .trace = trace, .wd_slot = null, .window = &.{} };
        }

        /// After INIT negotiated, before the session is ready.
        pub fn initFilesystem(self: *Self, negotiated: u64, minor: u32) Error!void {
            var trace = op_trace.OpTrace.begin(0, .other);
            defer trace.finish();
            var ctx = self.sessionCtx(&trace);
            try self.fs.vtable.init(self.fs.impl, &ctx, .{ .negotiated = negotiated, .minor = minor });
        }

        /// The session's teardown hook (DESTROY arrived, or the connection
        /// died): the filesystem's dismount sweep.
        pub fn destroyFilesystem(self: *Self) void {
            var trace = op_trace.OpTrace.begin(0, .other);
            defer trace.finish();
            var ctx = self.sessionCtx(&trace);
            self.fs.vtable.destroy(self.fs.impl, &ctx, .{});
        }

        /// The worker-side FORGET hand-off (one FORGET or one BATCH_FORGET
        /// entry, before the transport's auto-commit).
        pub fn forget(self: *Self, ino: u64, nlookup: u64) void {
            fuse_forgets.inc();
            self.forgetOne(.{ .unique = 0, .nodeid = ino, .uid = 0, .gid = 0, .pid = 0 }, ino, nlookup);
        }

        /// FORGET / one BATCH_FORGET entry: the table's exact return, then
        /// the filesystem's `forget` once per ino (the eviction fires on the
        /// LAST return of a tracked ino, so a batch's duplicates sum there).
        fn forgetOne(self: *Self, meta: Meta, ino: u64, nlookup: u64) void {
            const v = self.opens.returnLookups(ino, nlookup);
            if (v.evicted) fuse_forget_evictions.inc();
            var trace = op_trace.OpTrace.begin(meta.unique, .other);
            defer trace.finish();
            var ctx: Ctx = .{ .unique = meta.unique, .uid = meta.uid, .gid = meta.gid, .pid = meta.pid, .io = self.io, .trace = &trace, .wd_slot = null, .window = &.{} };
            self.fs.vtable.forget(self.fs.impl, &ctx, .{ .ino = ino, .nlookup = nlookup, .evicted = v.evicted });
        }

        fn noteEntry(self: *Self, e: filesystem.Entry) Error!void {
            if (e.isNegative()) return;
            try self.opens.noteLookup(e.nodeid);
        }

        /// READDIRPLUS: every positive entry in the packed window is a
        /// kernel lookup reference (FUSE-3k).
        fn noteDirentPlus(self: *Self, window: []const u8) Error!void {
            var off: usize = 0;
            while (off + abi.fuse_direntplus.NAME_OFFSET <= window.len) {
                const rec = abi.decodePrefix(abi.fuse_direntplus, window[off..]).?;
                if (rec.entry_out.nodeid != 0) try self.opens.noteLookup(rec.entry_out.nodeid);
                off += abi.fuse_direntplus.recordLen(rec.dirent.namelen);
            }
        }

        /// The held zero-copy payload's portable face (null: the bytes are
        /// in `data`, or this transport has no held leases). `ctx` points
        /// INTO the caller's lease, which outlives the filesystem's write
        /// (the lease is released after the reply) — never into a copy.
        fn heldPayload(lease: *const ?Transport.PayloadLease) ?filesystem.HeldPayload {
            if (Transport.PayloadLease != transport_slot.PayloadLease) return null;
            if (lease.* == null) return null;
            const l: *const Transport.PayloadLease = &lease.*.?;
            if (!l.held) return null;
            const Thunk = struct {
                fn extract(ctx: ?*anyopaque, io: Io) anyerror![]const u8 {
                    const lp: *const transport_slot.PayloadLease = @ptrCast(@alignCast(ctx.?));
                    return lp.bytes(io);
                }
            };
            return .{ .src = l.heldSource().?, .class = l.class, .ctx = @ptrCast(@constCast(l)), .extract = &Thunk.extract };
        }

        fn run(self: *Self, req: codec.Request, meta: Meta, c: *Ctx, out: *codec.OutBuf, lease: ?Transport.PayloadLease) Error!codec.Reply {
            const fs = self.fs;
            const u = meta.unique;
            const ino = meta.nodeid;
            switch (req) {
                .lookup => |a| {
                    const e = try fs.vtable.lookup(fs.impl, c, .{ .parent = ino, .name = a.name });
                    try self.noteEntry(e);
                    return codec.replyEntry(out, u, e);
                },
                .getattr => |a| return codec.replyAttr(out, u, try fs.vtable.getattr(fs.impl, c, .{ .ino = ino, .fh = a.fh })),
                .setattr => |in| {
                    const attr = filesystem.SetAttr.fromWire(in, @bitCast(clock.realtimeCoarseNs()));
                    // A size change is the whole-truncate exclusive guard
                    // (taker list: "SETATTR iff size"); times/mode/owner
                    // ride the 4a guards alone.
                    const g: ?op_guard.Guard = if (attr.size != null) self.guards.acquire(self.io, ino, .exclusive) else null;
                    defer if (g) |gg| gg.release(self.io);
                    return codec.replyAttr(out, u, try fs.vtable.setattr(fs.impl, c, .{ .ino = ino, .attr = attr }));
                },
                .readlink => {
                    const d = try fs.vtable.readlink(fs.impl, c, .{ .ino = ino });
                    return codec.replyData(u, c.window[0..d.len]);
                },
                .symlink => |a| {
                    const e = try fs.vtable.symlink(fs.impl, c, .{ .parent = ino, .name = a.name, .link = a.link });
                    try self.noteEntry(e);
                    return codec.replyEntry(out, u, e);
                },
                .mknod => |a| {
                    const e = try fs.vtable.mknod(fs.impl, c, .{ .parent = ino, .name = a.name, .mode = a.mode, .rdev = a.rdev, .umask = a.umask });
                    try self.noteEntry(e);
                    return codec.replyEntry(out, u, e);
                },
                .mkdir => |a| {
                    const e = try fs.vtable.mkdir(fs.impl, c, .{ .parent = ino, .name = a.name, .mode = a.mode, .umask = a.umask });
                    try self.noteEntry(e);
                    return codec.replyEntry(out, u, e);
                },
                .unlink => |a| {
                    try fs.vtable.unlink(fs.impl, c, .{ .parent = ino, .name = a.name });
                    return codec.replyOk(u);
                },
                .rmdir => |a| {
                    try fs.vtable.rmdir(fs.impl, c, .{ .parent = ino, .name = a.name });
                    return codec.replyOk(u);
                },
                .rename, .rename2 => |a| {
                    try fs.vtable.rename(fs.impl, c, .{ .parent = ino, .name = a.name, .newparent = a.newdir, .newname = a.newname, .flags = a.flags });
                    return codec.replyOk(u);
                },
                .link => |a| {
                    const e = try fs.vtable.link(fs.impl, c, .{ .ino = a.oldnodeid, .newparent = ino, .newname = a.newname });
                    try self.noteEntry(e);
                    return codec.replyEntry(out, u, e);
                },
                .open => |a| {
                    // ATOMIC_O_TRUNC: the kernel sends no SETATTR(size=0),
                    // so an O_TRUNC open IS the size set and takes the
                    // size setter's whole-truncate exclusive guard (taker
                    // list: "SETATTR iff size"); without it the truncate
                    // ran beside Shared-class writes admitted on `end ≤
                    // size` (`write_shared_class_violation`, fstests pass 2).
                    const g: ?op_guard.Guard = if (a.flags & filesystem.O_TRUNC != 0) self.guards.acquire(self.io, ino, .exclusive) else null;
                    defer if (g) |gg| gg.release(self.io);
                    const o = try fs.vtable.open(fs.impl, c, .{ .ino = ino, .flags = a.flags, .open_flags = a.open_flags });
                    _ = try self.opens.addOpen(ino);
                    return codec.replyOpen(out, u, o.fh, o.open_flags);
                },
                .opendir => |a| {
                    const o = try fs.vtable.opendir(fs.impl, c, .{ .ino = ino, .flags = a.flags, .open_flags = a.open_flags });
                    _ = try self.opens.addOpen(ino);
                    return codec.replyOpen(out, u, o.fh, o.open_flags);
                },
                .create => |a| {
                    const cr = try fs.vtable.create(fs.impl, c, .{ .parent = ino, .name = a.name, .mode = a.mode, .flags = a.flags, .umask = a.umask, .open_flags = a.open_flags });
                    try self.noteEntry(cr.entry);
                    _ = try self.opens.addOpen(cr.entry.nodeid);
                    return codec.replyCreate(out, u, cr.entry, cr.fh, cr.open_flags);
                },
                .read => |r| {
                    const d = try fs.vtable.read(fs.impl, c, .{ .ino = ino, .fh = r.fh, .offset = r.offset, .size = r.size, .flags = r.flags });
                    // A short reply zero-completes the window to the
                    // request's size (the copy path's page_zeroing; under
                    // zero-copy the bytes are already the kernel's). The
                    // O_DIRECT flag is the FILE's, not the path's: an mmap
                    // fault or readahead of an O_DIRECT fd is a page-cache
                    // fill that owes the same zeros (fstests generic/263),
                    // and a direct read's user buffer is `size` long.
                    var reply = codec.replyRead(u, c.window[0..d.len], r.size);
                    reply.zero_pad = codec.readZeroTail(c.window, reply.body.len, r.size);
                    return reply;
                },
                .write => |w| {
                    // R2-M2: a zero-length write has no side effects at all.
                    if (w.in.size == 0) return codec.replyWrite(out, u, 0);
                    const end = try op_guard.validateRange(w.in.offset, w.in.size, MAX_FILE_SIZE);
                    var adm = op_guard.admitWrite(self.guards, self.io, self.probe, self.shared_enabled, ino, w.in.offset, end);
                    defer adm.release(self.io);
                    // The classifier's verdict over the data plane's RAM
                    // snapshot (a resident striped write within the size
                    // floor is Shared; everything else is exclusive).
                    const candidate: op_guard.Class = if (adm.shared_snapshot) |snap| op_guard.classify(snap.classifyInput(end)) else .entire_op;
                    const class = adm.finalize(candidate);
                    c.stage(.write_locked);
                    const wr = try fs.vtable.write(fs.impl, c, .{ .ino = ino, .fh = w.in.fh, .offset = w.in.offset, .data = w.data, .held = heldPayload(&lease), .class = class, .write_flags = w.in.write_flags, .flags = w.in.flags });
                    self.opens.markDirty(ino);
                    return codec.replyWrite(out, u, wr.size);
                },
                .flush => |a| {
                    if (!self.opens.isDirty(ino)) {
                        fuse_flush_clean_fastpath.inc();
                        return error.NotImplemented;
                    }
                    try fs.vtable.flush(fs.impl, c, .{ .ino = ino, .fh = a.fh, .lock_owner = a.lock_owner });
                    return codec.replyOk(u);
                },
                .release => |a| {
                    if (!self.opens.isDirty(ino)) fuse_release_clean_fastpath.inc();
                    // Errors never reach close(2): the count drops whatever the
                    // filesystem answered, so "last close" is exact.
                    const r = fs.vtable.release(fs.impl, c, .{ .ino = ino, .fh = a.fh, .flags = a.flags, .lock_owner = a.lock_owner, .flush = a.release_flags & abi.FUSE_RELEASE_FLUSH != 0 });
                    _ = self.opens.removeOpen(ino);
                    try r;
                    return codec.replyOk(u);
                },
                .releasedir => |a| {
                    const r = fs.vtable.releasedir(fs.impl, c, .{ .ino = ino, .fh = a.fh, .flags = a.flags, .lock_owner = a.lock_owner, .flush = false });
                    _ = self.opens.removeOpen(ino);
                    try r;
                    return codec.replyOk(u);
                },
                .fsync => |a| {
                    try fs.vtable.fsync(fs.impl, c, .{ .ino = ino, .fh = a.fh, .datasync = a.datasync });
                    return codec.replyOk(u);
                },
                .fsyncdir => |a| {
                    try fs.vtable.fsyncdir(fs.impl, c, .{ .ino = ino, .fh = a.fh, .datasync = a.datasync });
                    return codec.replyOk(u);
                },
                .syncfs => {
                    try fs.vtable.syncfs(fs.impl, c, .{ .ino = ino });
                    return codec.replyOk(u);
                },
                .readdir => |r| {
                    const d = try fs.vtable.readdir(fs.impl, c, .{ .ino = ino, .fh = r.fh, .offset = r.offset, .size = r.size });
                    return codec.replyData(u, c.window[0..d.len]);
                },
                .readdirplus => |r| {
                    const d = try fs.vtable.readdirplus(fs.impl, c, .{ .ino = ino, .fh = r.fh, .offset = r.offset, .size = r.size });
                    try self.noteDirentPlus(c.window[0..d.len]);
                    return codec.replyData(u, c.window[0..d.len]);
                },
                .statfs => return codec.replyStatfs(out, u, try fs.vtable.statfs(fs.impl, c, .{ .ino = ino })),
                .setxattr => |a| {
                    try fs.vtable.setxattr(fs.impl, c, .{ .ino = ino, .name = a.name, .value = a.value, .flags = a.flags });
                    return codec.replyOk(u);
                },
                .getxattr => |a| return xattrReply(out, u, try fs.vtable.getxattr(fs.impl, c, .{ .ino = ino, .name = a.name, .size = a.size }), c.window),
                .listxattr => |a| return xattrReply(out, u, try fs.vtable.listxattr(fs.impl, c, .{ .ino = ino, .size = a.size }), c.window),
                .removexattr => |a| {
                    try fs.vtable.removexattr(fs.impl, c, .{ .ino = ino, .name = a.name });
                    return codec.replyOk(u);
                },
                .access => |a| {
                    try fs.vtable.access(fs.impl, c, .{ .ino = ino, .mask = a.mask });
                    return codec.replyOk(u);
                },
                .bmap => |a| {
                    const b = try fs.vtable.bmap(fs.impl, c, .{ .ino = ino, .blocksize = a.blocksize, .block = a.block });
                    return codec.replyBmap(out, u, b.block);
                },
                .ioctl => |a| {
                    const r = try fs.vtable.ioctl(fs.impl, c, .{ .ino = ino, .fh = a.in.fh, .flags = a.in.flags, .cmd = a.in.cmd, .arg = a.in.arg, .in = a.data, .out_size = a.in.out_size });
                    return codec.replyIoctl(out, u, r.out, c.window[0..r.data_len]);
                },
                .poll => |p| {
                    const r = try fs.vtable.poll(fs.impl, c, .{ .ino = ino, .fh = p.fh, .kh = p.kh, .flags = p.flags, .events = p.events });
                    return codec.replyPoll(out, u, r.revents);
                },
                .fallocate => |a| {
                    try fs.vtable.fallocate(fs.impl, c, .{ .ino = ino, .fh = a.fh, .offset = a.offset, .length = a.length, .mode = a.mode });
                    return codec.replyOk(u);
                },
                .lseek => |a| {
                    const r = try fs.vtable.lseek(fs.impl, c, .{ .ino = ino, .fh = a.fh, .offset = a.offset, .whence = a.whence });
                    return codec.replyLseek(out, u, r.offset);
                },
                .copy_file_range => |a| {
                    const r = try fs.vtable.copy_file_range(fs.impl, c, .{ .ino_in = ino, .fh_in = a.fh_in, .off_in = a.off_in, .ino_out = a.nodeid_out, .fh_out = a.fh_out, .off_out = a.off_out, .len = a.len, .flags = a.flags });
                    return codec.replyWrite(out, u, r.size);
                },
                .destroy => {
                    fs.vtable.destroy(fs.impl, c, .{});
                    return codec.replyOk(u);
                },
                .init => {
                    fuse_duplicate_inits.inc();
                    log.warn("FUSE_INIT on a live session (unique {d}): the kernel negotiates once per connection — refusing EINVAL", .{u});
                    return error.InvalidArgument;
                },
                // POSIX_LOCKS/FLOCK_LOCKS are never advertised: the kernel
                // arbitrates locks itself and never sends these.
                .getlk, .setlk, .setlkw, .unsupported => {
                    fuse_unsupported_opcodes.inc();
                    return error.NotImplemented;
                },
                .forget, .batch_forget, .interrupt, .notify_reply => unreachable, // routed before `run`
            }
        }

        fn xattrReply(out: *codec.OutBuf, unique: u64, x: filesystem.Xattr, window: []const u8) codec.Reply {
            return switch (x) {
                .size => |n| codec.replyXattrSize(out, unique, n),
                .data => |len| codec.replyData(unique, window[0..len]),
            };
        }
    };
}

/// The opcode of a decoded request (the watchdog kind; every opcode
/// registers — ledger L14).
pub fn opcodeOf(req: codec.Request) abi.Opcode {
    return switch (req) {
        .unsupported => |op| op,
        inline else => |_, tag| @field(abi.Opcode, @tagName(tag)),
    };
}

// ---------------------------------------------------------------------------
// Tests: the lifecycle over the loopback transport and a scripted
// filesystem (the POSIX suite over a real filesystem is `kv_fs.zig`'s).
// ---------------------------------------------------------------------------

const testing = std.testing;
const Loopback = seam.Loopback;
const LoopDispatcher = Dispatcher(Loopback);

/// A filesystem whose every op is scripted by a verdict word, so each
/// lifecycle arm is reachable deterministically.
const Scripted = struct {
    verdict: Error!void = {},
    forgets: u32 = 0,
    last_forget: ?filesystem.ForgetArgs = null,
    destroyed: u32 = 0,
    entry_ino: u64 = 50,
    /// A hook run inside `write` (the INTERRUPT test parks here).
    write_hook: ?*const fn (*Scripted) void = null,
    writes: u32 = 0,
    /// A READ serves this many bytes (the window beyond is left as found).
    short_read: ?usize = null,
    syncfs_calls: u32 = 0,
    syncfs_inos: [2]u64 = @splat(0),

    fn entry(self: *Scripted) filesystem.Entry {
        var a = abi.zeroed(abi.fuse_attr);
        a.ino = self.entry_ino;
        a.mode = 0o100644;
        return .{ .nodeid = self.entry_ino, .entry_ttl_ns = 1_000_000_000, .attr_ttl_ns = 500_000_000, .attr = a };
    }

    pub fn lookup(self: *Scripted, c: *Ctx, _: filesystem.LookupArgs) Error!filesystem.Entry {
        c.stage(.meta_backend_start);
        try self.verdict;
        c.stage(.meta_backend_done);
        return self.entry();
    }
    pub fn create(self: *Scripted, _: *Ctx, _: filesystem.CreateArgs) Error!filesystem.Created {
        try self.verdict;
        return .{ .entry = self.entry(), .fh = 9, .open_flags = abi.FOPEN_KEEP_CACHE };
    }
    pub fn open(self: *Scripted, _: *Ctx, _: filesystem.OpenArgs) Error!filesystem.Opened {
        try self.verdict;
        return .{ .fh = 9, .open_flags = 0 };
    }
    pub fn release(self: *Scripted, _: *Ctx, _: filesystem.ReleaseArgs) Error!void {
        try self.verdict;
    }
    pub fn flush(self: *Scripted, _: *Ctx, _: filesystem.FlushArgs) Error!void {
        try self.verdict;
    }
    pub fn read(self: *Scripted, c: *Ctx, a: filesystem.ReadArgs) Error!filesystem.Data {
        try self.verdict;
        const n = @min(self.short_read orelse a.size, c.window.len);
        @memset(c.window[0..n], 0xAB);
        return .{ .len = n };
    }
    pub fn write(self: *Scripted, _: *Ctx, a: filesystem.WriteArgs) Error!filesystem.Written {
        if (self.write_hook) |h| h(self);
        try self.verdict;
        self.writes += 1;
        return .{ .size = @intCast(a.data.len) };
    }
    pub fn readdirplus(self: *Scripted, c: *Ctx, a: filesystem.ReaddirArgs) Error!filesystem.Data {
        try self.verdict;
        var w = codec.DirentPlusWriter.init(c.window, a.size);
        var dot = self.entry();
        dot.nodeid = 0;
        w.add(dot, 1, ".") catch return error.InputOutput;
        var e1 = self.entry();
        e1.nodeid = 60;
        e1.attr.ino = 60;
        var e2 = self.entry();
        e2.nodeid = 61;
        e2.attr.ino = 61;
        w.add(e1, 3, "one") catch return error.InputOutput;
        w.add(e2, 4, "two") catch return error.InputOutput;
        return .{ .len = w.len };
    }
    pub fn syncfs(self: *Scripted, _: *Ctx, a: filesystem.SyncfsArgs) Error!void {
        try self.verdict;
        self.syncfs_inos[self.syncfs_calls % self.syncfs_inos.len] = a.ino;
        self.syncfs_calls += 1;
    }
    pub fn forget(self: *Scripted, _: *Ctx, a: filesystem.ForgetArgs) void {
        self.forgets += 1;
        self.last_forget = a;
    }
    pub fn destroy(self: *Scripted, _: *Ctx, _: filesystem.DestroyArgs) void {
        self.destroyed += 1;
    }
};

const Rig = struct {
    lb: Loopback,
    reg: watchdog.Registry,
    guards: op_guard.Table,
    opens: open_table.OpenTable,
    fsi: Scripted = .{},
    d: LoopDispatcher = undefined,

    fn init(self: *Rig) !void {
        self.lb = try Loopback.init(testing.allocator, 8, 4096);
        self.reg = try watchdog.Registry.init(testing.allocator, 1);
        self.guards = try op_guard.Table.init(testing.allocator, 64);
        self.opens = try open_table.OpenTable.init(testing.allocator, 64);
        self.fsi = .{};
        self.d = LoopDispatcher.init(&self.lb, .{ .fs = Filesystem.of(Scripted, &self.fsi), .io = testing.io, .registry = &self.reg, .guards = &self.guards, .opens = &self.opens });
    }

    fn deinit(self: *Rig) void {
        self.opens.deinit();
        self.guards.deinit();
        self.reg.deinit();
        self.lb.deinit();
    }

    fn meta(unique: u64, nodeid: u64) Meta {
        return .{ .unique = unique, .nodeid = nodeid, .uid = 1000, .gid = 1000, .pid = 42 };
    }
};

test "every reply-owing arm commits exactly once: success, a handler errno through the one table, ENOSYS for an unserved opcode, EINVAL for a duplicate INIT, OK for DESTROY" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const ops0 = fuse_ops.load();
    const s0 = Loopback.slot(0);
    rig.d.dispatch(.{ .lookup = .{ .name = "a" } }, Rig.meta(2, 1), s0, null);
    try testing.expectEqual(@as(u32, 1), rig.lb.replyCount(s0));
    const e = rig.lb.outAs(s0, abi.fuse_entry_out).?;
    try testing.expectEqual(@as(u64, 50), e.nodeid);
    try testing.expectEqual(@as(u64, 1), rig.opens.get(50).?.lookups);
    // A handler error: its errno, header-only.
    rig.fsi.verdict = error.FileNotFound;
    rig.d.dispatch(.{ .lookup = .{ .name = "b" } }, Rig.meta(4, 1), s0, null);
    try testing.expectEqual(@as(u32, 2), rig.lb.replyCount(s0));
    try testing.expectEqual(linux.E.NOENT, rig.lb.captured(s0).?.errno().?);
    try testing.expectEqual(@as(u64, 1), rig.opens.get(50).?.lookups);
    rig.fsi.verdict = {};
    // Unserved: ENOSYS; the kernel latches.
    const un0 = fuse_unsupported_opcodes.load();
    rig.d.dispatch(.{ .getattr = .{ .flags = 0, .fh = null } }, Rig.meta(6, 50), s0, null);
    try testing.expectEqual(linux.E.NOSYS, rig.lb.captured(s0).?.errno().?);
    rig.d.dispatch(.{ .unsupported = abi.Opcode.fromInt(52) }, Rig.meta(8, 1), s0, null);
    try testing.expectEqual(linux.E.NOSYS, rig.lb.captured(s0).?.errno().?);
    try testing.expectEqual(un0 + 1, fuse_unsupported_opcodes.load());
    rig.d.dispatch(.{ .init = .{ .in = abi.zeroed(abi.fuse_init_in), .raw_len = 16 } }, Rig.meta(10, 0), s0, null);
    try testing.expectEqual(linux.E.INVAL, rig.lb.captured(s0).?.errno().?);
    rig.d.dispatch(.destroy, Rig.meta(12, 0), s0, null);
    try testing.expectEqual(@as(?linux.E, null), rig.lb.captured(s0).?.errno());
    try testing.expectEqual(@as(u32, 1), rig.fsi.destroyed);
    try testing.expectEqual(@as(u32, 6), rig.lb.replyCount(s0));
    try testing.expectEqual(ops0 + 6, fuse_ops.load());
    try testing.expectEqual(@as(u32, 0), rig.reg.inFlight());
    try testing.expectEqual(@as(u64, 0), counters.tripwireCount("fuse_replies_synthesized"));
    try testing.expectEqual(@as(u64, 0), counters.tripwireCount("fuse_reply_twice"));
}

/// A classical device read of FUSE_SYNCFS (opcode 50): the header on the
/// root's nodeid and `fuse_syncfs_in{padding}` — what `fuse_sync_fs` sends
/// on syncfs(2) once the connection propagates it.
fn syncfsRequest(buf: *[64]u8, unique: u64) !codec.Request {
    const body_len = 8;
    buf[0..40].* = abi.encode(abi.fuse_in_header{ .len = 40 + body_len, .opcode = 50, .unique = unique, .nodeid = abi.FUSE_ROOT_ID, .uid = 0, .gid = 0, .pid = 4242, .total_extlen = 0, .padding = 0 });
    @memset(buf[40..][0..body_len], 0);
    return codec.parse(try codec.Delivery.fromClassical(buf, 40 + body_len));
}

test "FUSE_SYNCFS (opcode 50) decodes and reaches the filesystem's syncfs once with the request's nodeid, replying OK; a handler error is its errno; a filesystem without syncfs answers ENOSYS (the kernel clears fc->sync_fs and stops asking)" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const s = Loopback.slot(2);
    var buf: [64]u8 = undefined;
    const un0 = fuse_unsupported_opcodes.load();
    rig.d.dispatch(try syncfsRequest(&buf, 2), Rig.meta(2, abi.FUSE_ROOT_ID), s, null);
    try testing.expectEqual(@as(u32, 1), rig.lb.replyCount(s));
    try testing.expectEqual(@as(?linux.E, null), rig.lb.captured(s).?.errno());
    try testing.expectEqual(@as(u32, 1), rig.fsi.syncfs_calls);
    try testing.expectEqual(abi.FUSE_ROOT_ID, rig.fsi.syncfs_inos[0]);
    try testing.expectEqual(un0, fuse_unsupported_opcodes.load());
    // A failed drain is the application's syncfs(2) errno.
    rig.fsi.verdict = error.InputOutput;
    rig.d.dispatch(try syncfsRequest(&buf, 4), Rig.meta(4, abi.FUSE_ROOT_ID), s, null);
    try testing.expectEqual(linux.E.IO, rig.lb.captured(s).?.errno().?);
    rig.fsi.verdict = {};
    // No syncfs in the filesystem: ENOSYS.
    const Bare = struct {};
    var bare: Bare = .{};
    var d2 = LoopDispatcher.init(&rig.lb, .{ .fs = Filesystem.of(Bare, &bare), .io = testing.io, .registry = &rig.reg, .guards = &rig.guards, .opens = &rig.opens });
    d2.dispatch(try syncfsRequest(&buf, 6), Rig.meta(6, abi.FUSE_ROOT_ID), s, null);
    try testing.expectEqual(linux.E.NOSYS, rig.lb.captured(s).?.errno().?);
    try testing.expectEqual(@as(u32, 3), rig.lb.replyCount(s));
    try testing.expectEqual(@as(u32, 0), rig.reg.inFlight());
}

test "FORGET and BATCH_FORGET never reply; the filesystem's forget runs once per entry with the table's verdict; INTERRUPT is counted and ignored; NOTIFY_REPLY is silent" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const s = Loopback.slot(1);
    rig.d.dispatch(.{ .lookup = .{ .name = "a" } }, Rig.meta(2, 1), s, null);
    rig.d.dispatch(.{ .lookup = .{ .name = "a" } }, Rig.meta(4, 1), s, null);
    try testing.expectEqual(@as(u64, 2), rig.opens.get(50).?.lookups);
    const int0 = fuse_interrupts_received.load();
    const f0 = fuse_forgets.load();
    const ev0 = fuse_forget_evictions.load();
    // A partial forget: not evicted, nothing replied.
    rig.d.dispatch(.{ .forget = .{ .nlookup = 1 } }, Rig.meta(6, 50), s, null);
    try testing.expectEqual(@as(u32, 1), rig.fsi.forgets);
    try testing.expect(!rig.fsi.last_forget.?.evicted);
    try testing.expectEqual(@as(u64, 1), rig.opens.get(50).?.lookups);
    // The batch carries the rest plus an untracked ino.
    const entries = [_]abi.fuse_forget_one{ .{ .nodeid = 50, .nlookup = 1 }, .{ .nodeid = 777, .nlookup = 3 } };
    rig.d.dispatch(.{ .batch_forget = .{ .entries = &entries } }, Rig.meta(8, 0), s, null);
    try testing.expectEqual(@as(u32, 3), rig.fsi.forgets);
    try testing.expect(rig.fsi.last_forget.?.evicted);
    try testing.expectEqual(@as(u64, 777), rig.fsi.last_forget.?.ino);
    try testing.expectEqual(@as(u64, 3), rig.fsi.last_forget.?.nlookup);
    try testing.expectEqual(@as(?open_table.Entry, null), rig.opens.get(50));
    try testing.expectEqual(f0 + 3, fuse_forgets.load());
    try testing.expectEqual(ev0 + 2, fuse_forget_evictions.load());
    rig.d.dispatch(.{ .interrupt = .{ .unique = 2 } }, Rig.meta(9, 0), s, null);
    rig.d.dispatch(.{ .notify_reply = .{ .offset = 0, .data = &.{} } }, Rig.meta(11, 0), s, null);
    try testing.expectEqual(int0 + 1, fuse_interrupts_received.load());
    // Two lookups replied; nothing else did.
    try testing.expectEqual(@as(u32, 2), rig.lb.replyCount(s));
}

test "open count: OPEN/CREATE count before the reply, RELEASE drops even on a handler error, clean handles take the fast paths and FLUSH answers ENOSYS; the WRITE lease is released before the reply" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const s = Loopback.slot(2);
    rig.d.dispatch(.{ .create = .{ .name = "f", .flags = 0, .mode = 0o644, .umask = 0, .open_flags = 0 } }, Rig.meta(2, 1), s, null);
    try testing.expectEqual(@as(u32, 1), rig.opens.get(50).?.count);
    try testing.expectEqual(@as(u64, 1), rig.opens.get(50).?.lookups);
    try testing.expectEqual(@as(usize, codec.MAX_OUT_LEN), rig.lb.captured(s).?.out_len);
    rig.d.dispatch(.{ .open = .{ .flags = 0, .open_flags = 0 } }, Rig.meta(4, 50), s, null);
    try testing.expectEqual(@as(u32, 2), rig.opens.get(50).?.count);
    // Clean: FLUSH is ENOSYS (the kernel's no_flush latch).
    const fl0 = fuse_flush_clean_fastpath.load();
    rig.d.dispatch(.{ .flush = .{ .fh = 9, .lock_owner = 0 } }, Rig.meta(6, 50), s, null);
    try testing.expectEqual(linux.E.NOSYS, rig.lb.captured(s).?.errno().?);
    try testing.expectEqual(fl0 + 1, fuse_flush_clean_fastpath.load());
    // A WRITE dirties the handle and releases its lease BEFORE the reply.
    const payload: [100]u8 = @splat(1);
    const win: abi.fuse_write_in = .{ .fh = 9, .offset = 0, .size = 100, .write_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    rig.d.dispatch(.{ .write = .{ .in = win, .data = &payload } }, Rig.meta(8, 50), s, .{ .idx = 2 });
    try testing.expectEqual(@as(u64, 1), rig.lb.leasesReleased());
    try testing.expectEqual(@as(u32, 100), rig.lb.outAs(s, abi.fuse_write_out).?.size);
    try testing.expect(rig.opens.isDirty(50));
    // A zero-length write: no handler call, no side effect, `written 0`.
    var zero = win;
    zero.size = 0;
    rig.d.dispatch(.{ .write = .{ .in = zero, .data = &.{} } }, Rig.meta(9, 50), s, null);
    try testing.expectEqual(@as(u32, 1), rig.fsi.writes);
    try testing.expectEqual(@as(u32, 0), rig.lb.outAs(s, abi.fuse_write_out).?.size);
    // A write past the 16 PiB ceiling is EFBIG before any guard.
    var far = win;
    far.offset = std.math.maxInt(u64) - 10;
    rig.d.dispatch(.{ .write = .{ .in = far, .data = &payload } }, Rig.meta(10, 50), s, null);
    try testing.expectEqual(linux.E.FBIG, rig.lb.captured(s).?.errno().?);
    // Dirty: FLUSH reaches the filesystem.
    rig.d.dispatch(.{ .flush = .{ .fh = 9, .lock_owner = 0 } }, Rig.meta(12, 50), s, null);
    try testing.expectEqual(@as(?linux.E, null), rig.lb.captured(s).?.errno());
    // RELEASE drops the count even when the filesystem errors (close(2)
    // never sees it; the count must stay exact).
    const rc0 = fuse_release_clean_fastpath.load();
    rig.fsi.verdict = error.InputOutput;
    rig.d.dispatch(.{ .release = .{ .fh = 9, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, Rig.meta(14, 50), s, null);
    try testing.expectEqual(linux.E.IO, rig.lb.captured(s).?.errno().?);
    try testing.expectEqual(@as(u32, 1), rig.opens.get(50).?.count);
    rig.fsi.verdict = {};
    rig.d.dispatch(.{ .release = .{ .fh = 9, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, Rig.meta(16, 50), s, null);
    try testing.expectEqual(@as(u32, 0), rig.opens.get(50).?.count);
    try testing.expectEqual(rc0, fuse_release_clean_fastpath.load());
    // The lookup reference is still the kernel's; the final FORGET evicts
    // with nothing stranded.
    const stranded0 = counters.tripwireCount("open_count_stranded");
    rig.d.dispatch(.{ .forget = .{ .nlookup = 1 } }, Rig.meta(18, 50), s, null);
    try testing.expectEqual(@as(?open_table.Entry, null), rig.opens.get(50));
    try testing.expectEqual(stranded0, counters.tripwireCount("open_count_stranded"));
    // The write admission ledger closed on the one write that reached the guard.
    try testing.expectEqual(@as(u32, 0), rig.guards.locks[rig.guards.stripeOf(50)].readers);
}

test "READDIRPLUS counts one lookup reference per positive entry (never the nodeid-0 dots); READ truncates the window to the request size" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const s = Loopback.slot(3);
    const rin: abi.fuse_read_in = .{ .fh = 1, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    rig.d.dispatch(.{ .readdirplus = rin }, Rig.meta(2, 1), s, null);
    try testing.expectEqual(@as(?linux.E, null), rig.lb.captured(s).?.errno());
    try testing.expectEqual(@as(u64, 1), rig.opens.get(60).?.lookups);
    try testing.expectEqual(@as(u64, 1), rig.opens.get(61).?.lookups);
    try testing.expectEqual(@as(?open_table.Entry, null), rig.opens.get(0));
    var small = rin;
    small.size = 10;
    rig.d.dispatch(.{ .read = small }, Rig.meta(4, 60), s, null);
    try testing.expectEqual(@as(usize, 10), rig.lb.captured(s).?.body.len);
    try testing.expectEqual(@as(u8, 0xAB), rig.lb.captured(s).?.body[9]);
}

test "a SHORT paged READ zero-completes the requested bytes past the served ones in the reply window, bounded by the request's size (the kernel copy path's page_zeroing, owed by the daemon under zero-copy) — for an O_DIRECT-flagged file too: `fuse_read_in.flags` is the FILE's open flags and a page-cache fill (an mmap fault, readahead) of an O_DIRECT fd carries them (repro-port: LTP fs gf18; fstests generic/263 fsx -Z MAPREAD read 0xaaaa past EOF)" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    const s = Loopback.slot(5);
    const win = rig.lb.replyWindow(s);
    // The filesystem serves 1,959 bytes of a 4,096-byte request into a
    // window full of yesterday's bytes.
    @memset(win, 0x5A);
    rig.fsi.short_read = 1959;
    const rin: abi.fuse_read_in = .{ .fh = 1, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    rig.d.dispatch(.{ .read = rin }, Rig.meta(7, 60), s, null);
    try testing.expectEqual(@as(usize, 1959), rig.lb.captured(s).?.body.len);
    try testing.expect(std.mem.allEqual(u8, win[0..1959], 0xAB));
    try testing.expect(std.mem.allEqual(u8, win[1959..4096], 0));
    // O_DIRECT in the flags: the same pad, to `size` and never past it (a
    // direct read's user buffer is exactly `size` long; a page-cache fill's
    // page is the installed window).
    @memset(win, 0x5A);
    var direct = rin;
    direct.flags = @bitCast(linux.O{ .DIRECT = true });
    direct.size = 3072;
    rig.d.dispatch(.{ .read = direct }, Rig.meta(8, 60), s, null);
    try testing.expectEqual(@as(usize, 1959), rig.lb.captured(s).?.body.len);
    try testing.expect(std.mem.allEqual(u8, win[1959..3072], 0));
    try testing.expect(std.mem.allEqual(u8, win[3072..4096], 0x5A));
}

test "the obligation: a second reply is dropped and counted (never an overwrite); an owed obligation's deinit synthesizes header-only EIO and counts" {
    var lb = try Loopback.init(testing.allocator, 1, 64);
    defer lb.deinit();
    const s = Loopback.slot(0);
    const twice0 = counters.tripwireCount("fuse_reply_twice");
    const syn0 = counters.tripwireCount("fuse_replies_synthesized");
    {
        var ob: LoopDispatcher.Obligation = .{ .transport = &lb, .slot = s, .unique = 5 };
        defer ob.deinit();
        ob.reply(codec.replyOk(5));
        fuse_reply_twice.resetLogBudgetForTest();
        log.testing.expectErrorLogs(1);
        ob.fail(.IO);
        try log.testing.expectAllFired();
        try testing.expectEqual(@as(u32, 1), lb.replyCount(s));
        try testing.expectEqual(@as(?linux.E, null), lb.captured(s).?.errno());
    }
    try testing.expectEqual(twice0 + 1, counters.tripwireCount("fuse_reply_twice"));
    {
        var ob: LoopDispatcher.Obligation = .{ .transport = &lb, .slot = s, .unique = 7 };
        fuse_replies_synthesized.resetLogBudgetForTest();
        log.testing.expectErrorLogs(1);
        ob.deinit();
        try log.testing.expectAllFired();
    }
    try testing.expectEqual(syn0 + 1, counters.tripwireCount("fuse_replies_synthesized"));
    try testing.expectEqual(@as(u32, 2), lb.replyCount(s));
    try testing.expectEqual(linux.E.IO, lb.captured(s).?.errno().?);
    try testing.expectEqual(@as(u64, 7), lb.captured(s).?.header.unique);
}

/// The INTERRUPT law, red-first for the reference (which has no test): a
/// WRITE parked mid-commit receives an INTERRUPT for its unique; (a) no
/// reply is ever written for the INTERRUPT, (b) the parked op still
/// commits exactly once, (c) `fuse_interrupts_received` moved by one.
const parked_write = struct {
    var gate: std.atomic.Value(u32) = .init(0);
    var parked: std.atomic.Value(bool) = .init(false);

    fn hook(_: *Scripted) void {
        parked.store(true, .release);
        while (gate.load(.acquire) == 0) std.Thread.yield() catch {};
    }

    fn run(rig: *Rig) void {
        const payload: [16]u8 = @splat(7);
        const win: abi.fuse_write_in = .{ .fh = 9, .offset = 0, .size = 16, .write_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
        rig.d.dispatch(.{ .write = .{ .in = win, .data = &payload } }, Rig.meta(100, 50), Loopback.slot(4), .{ .idx = 4 });
    }
};

test "INTERRUPT of a WRITE parked mid-commit: no reply for the interrupt, the write still commits exactly once, the counter moves by one" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    rig.fsi.write_hook = &parked_write.hook;
    parked_write.gate.store(0, .monotonic);
    parked_write.parked.store(false, .monotonic);
    const t = try std.Thread.spawn(.{}, parked_write.run, .{&rig});
    while (!parked_write.parked.load(.acquire)) std.Thread.yield() catch {};
    // The op is registered with the watchdog and holds the exclusive guard.
    try testing.expectEqual(@as(u32, 1), rig.reg.inFlight());
    try testing.expect(rig.guards.locks[rig.guards.stripeOf(50)].writer);
    const int0 = fuse_interrupts_received.load();
    const int_slot = Loopback.slot(5);
    rig.d.dispatch(.{ .interrupt = .{ .unique = 100 } }, Rig.meta(101, 0), int_slot, null);
    try testing.expectEqual(int0 + 1, fuse_interrupts_received.load());
    try testing.expectEqual(@as(u32, 0), rig.lb.replyCount(int_slot));
    // Nothing was interrupted: the write is still parked, still owing.
    try testing.expectEqual(@as(u32, 0), rig.lb.replyCount(Loopback.slot(4)));
    try testing.expectEqual(@as(u32, 1), rig.reg.inFlight());
    parked_write.gate.store(1, .release);
    t.join();
    try testing.expectEqual(@as(u32, 1), rig.lb.replyCount(Loopback.slot(4)));
    try testing.expectEqual(@as(u32, 16), rig.lb.outAs(Loopback.slot(4), abi.fuse_write_out).?.size);
    try testing.expectEqual(@as(u32, 0), rig.lb.replyCount(int_slot));
    try testing.expectEqual(@as(u32, 0), rig.reg.inFlight());
    try testing.expectEqual(@as(u64, 1), rig.lb.leasesReleased());
}

test "profiling off: a dispatch allocates nothing and takes no phase stamps; on: the phases record" {
    var rig: Rig = undefined;
    try rig.init();
    defer rig.deinit();
    // Pre-touch the open table's entry so the lookup's `noteLookup` has
    // its slot (the table grows by doubling, never per op).
    try rig.opens.noteLookup(50);
    _ = rig.opens.returnLookups(50, 1);
    op_trace.setArmedForTest(false);
    defer op_trace.setArmedForTest(false);
    var failing = std.testing.FailingAllocator.init(testing.allocator, .{ .fail_index = 0 });
    rig.opens.gpa = failing.allocator();
    defer rig.opens.gpa = testing.allocator;
    const phase0 = op_trace.phaseHistogram(.lookup, .total).snapshot().count;
    const lat0 = op_trace.latencyHistogram(.lookup).snapshot().count;
    rig.d.dispatch(.{ .lookup = .{ .name = "a" } }, Rig.meta(2, 1), Loopback.slot(6), null);
    try testing.expectEqual(@as(?linux.E, null), rig.lb.captured(Loopback.slot(6)).?.errno());
    try testing.expectEqual(@as(usize, 0), failing.allocations);
    try testing.expectEqual(phase0, op_trace.phaseHistogram(.lookup, .total).snapshot().count);
    try testing.expectEqual(lat0 + 1, op_trace.latencyHistogram(.lookup).snapshot().count);
    op_trace.setArmedForTest(true);
    rig.d.dispatch(.{ .lookup = .{ .name = "a" } }, Rig.meta(4, 1), Loopback.slot(6), null);
    try testing.expectEqual(phase0 + 1, op_trace.phaseHistogram(.lookup, .total).snapshot().count);
    try testing.expectEqual(phase0 + 1, op_trace.phaseHistogram(.lookup, .backend).snapshot().count);
}

test "opcodeOf maps every request arm to its opcode" {
    try testing.expectEqual(abi.Opcode.lookup, opcodeOf(.{ .lookup = .{ .name = "" } }));
    try testing.expectEqual(abi.Opcode.rename2, opcodeOf(.{ .rename2 = .{ .newdir = 0, .flags = 0, .name = "", .newname = "" } }));
    try testing.expectEqual(abi.Opcode.destroy, opcodeOf(.destroy));
    try testing.expectEqual(abi.Opcode.fromInt(52), opcodeOf(.{ .unsupported = abi.Opcode.fromInt(52) }));
}

/// A transport with the REAL slot lease type (the Loopback's shape with the
/// held face): `heldPayload` must hand the filesystem a `ctx` that points
/// into the dispatcher's lease, never into a copy that dies with the frame.
const HeldLoopback = struct {
    pub const SlotRef = Loopback.SlotRef;
    pub const PayloadLease = transport_slot.PayloadLease;
    pub fn replyWindow(_: *HeldLoopback, _: SlotRef) []u8 {
        return &.{};
    }
    pub fn submitReply(_: *HeldLoopback, _: SlotRef, _: codec.Reply) void {}
    pub fn failEnt(_: *HeldLoopback, _: SlotRef, _: u64, _: linux.E) void {}
    pub fn releaseLease(_: *HeldLoopback, _: PayloadLease) void {}
};

test "heldPayload points INTO the caller's lease and its extract reaches the slot's face (a held write that no arm stores direct materializes through it)" {
    const Face = struct {
        var hits: u32 = 0;
        var last_gent: u32 = 0;
        const bytes = "held payload bytes";
        fn extract(_: ?*anyopaque, gent: u32, _: Io) transport_slot.ExtractError![]const u8 {
            hits += 1;
            last_gent = gent;
            return bytes;
        }
        fn wake(_: ?*anyopaque) bool {
            return false;
        }
    };
    var state: transport_slot.LeaseState = .{};
    var lease: ?transport_slot.PayloadLease = transport_slot.PayloadLease.acquireHeld(&state, .{ .ctx = null, .wake = &Face.wake }, 4096, .page_cache, 2, 5, 7, .{ .ctx = null, .extract = &Face.extract });
    const held = Dispatcher(HeldLoopback).heldPayload(&lease).?;
    try testing.expectEqual(@as(*anyopaque, @ptrCast(&lease.?)), held.ctx.?);
    try testing.expectEqual(@as(u16, 2), held.src.qid);
    try testing.expectEqual(@as(u16, 5), held.src.buf_index);
    try testing.expectEqual(@as(u32, 4096), held.src.len);
    try testing.expectEqualStrings(Face.bytes, try held.extract(held.ctx, testing.io));
    try testing.expectEqual(@as(u32, 1), Face.hits);
    try testing.expectEqual(@as(u32, 7), Face.last_gent);
    lease.?.release();
    // A lease in hand (not held) has no held face.
    var plain: ?transport_slot.PayloadLease = transport_slot.PayloadLease.acquire(&state, "in hand", .{ .ctx = null, .wake = &Face.wake });
    try testing.expect(Dispatcher(HeldLoopback).heldPayload(&plain) == null);
    plain.?.release();
}
