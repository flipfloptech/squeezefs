//! The journal tail: a kernel-held mirror of a writable volume's journal
//! ring that outlives the daemon (charter §5.2 "The ack point"; decision
//! 0003 §BB.2–BB.3). Under the coalesced cadence a window's members are
//! acked once the RAM apply is published and the window's image is COPIED
//! here, before the landing; the mount's supervisor holds an fd to the tail
//! and, at the daemon's death, writes every un-landed slot to the device in
//! seq order and barriers before it releases the writer guard — a daemon
//! crash loses nothing acked.
//!
//! The shape (the staging ring's trick: a memfd the daemon writes with a
//! memcpy and another process reads): a header (magic, version, the device
//! path, the ring's segments and grain), a slot table, and the MIRROR — the
//! ring's bytes at the ring's own offsets. The applier copies the window's
//! aligned ops into the mirror at `op.offset − ring_base` and publishes the
//! slot (`written`, release) with the window's covering runs; the lane's
//! terminal outcome frees the slot. The bytes the lane writes ARE the
//! replay's bytes: one memcpy per window, no second codec, byte-identical
//! to the device image, so a landed window rewritten is idempotent by
//! construction and a slot freed is skipped.
//!
//! A slot's covering runs hold only bytes the window owns: on a padded
//! ring the runs are the window's aligned runs themselves; on an unpadded
//! ring the span from its first op to its last holds its entries, the page
//! headers it owns (every page boundary inside a window's logical span is
//! its own) and the holes of rolled-out members — never another window's
//! byte. A window with apply holes is acked at the landing (the hole
//! checkpoint must precede its survivors' ack), so a hole's zeros replayed
//! are never ahead of an acked survivor.
//!
//! A tail whose replay FAILED is HELD, never discarded (charter §5.2,
//! decision 0003 §BQ): the supervisor copies its written slots into a file
//! of the same image under the host's RAM runtime directory (`HeldDir`,
//! named by the volume's uuid), releases the writer guard, and retries the
//! replay under a non-blocking writer lock until the device answers; the
//! next writer of the volume replays the file under ITS lock before it
//! reads the ring (`consumeHeld`), and refuses to open while it cannot. The
//! header names the volume and its set, and every slot its reservation's
//! position, so a foreign tail is refused and a stale one (the ledger's
//! checkpoint tail past its first window: someone wrote the ring without
//! it) is never written over a ring it no longer describes.
//!
//! Portable: the image over a byte buffer (`Image`) and the replay over a
//! sink are tested on every host; the memfd, the device replay and the held
//! file are the Linux part (`Tail`, `replayFd`, `persistHeld`, `retryHeld`,
//! `consumeHeld`).

const std = @import("std");
const builtin = @import("builtin");

const journal = @import("journal.zig");
const superblock = @import("superblock.zig");
const knobs = @import("../core/knobs.zig");
const testkit = @import("../core/testkit.zig");

pub const MAGIC: [8]u8 = "SQZJTAIL".*;
/// v2: the volume's identity and the ring's kind in the header, the
/// reservation's position in every slot (decision 0003 §BQ).
pub const VERSION: u32 = 2;
/// The device path in the header: the kernel's own bound.
pub const PATH_CAP: usize = std.os.linux.PATH_MAX;
/// The mirror begins on a page: a replayer's `pwrite` of a page-aligned
/// run from a page-aligned source.
const MIRROR_ALIGN: u64 = journal.PAGE_LEN;

pub const Segment = extern struct { start: u64, pages: u64 };

/// Whose ring the tail mirrors: the volume's fixed ring (its D0 writer's,
/// opened under the writer lock — the ring a held tail's successor owns)
/// or an appender's ring on a joined volume (no lock of its own; a held
/// copy has no successor law yet, decision 0003 §BQ.5).
pub const RING_FIXED: u32 = 1;
pub const RING_JOINED: u32 = 2;

/// The volume a tail belongs to: its superblock's uuid and set uuid.
pub const Identity = struct {
    volume_uuid: [16]u8,
    set_uuid: [16]u8,

    pub fn eql(a: Identity, b: Identity) bool {
        return std.mem.eql(u8, &a.volume_uuid, &b.volume_uuid) and std.mem.eql(u8, &a.set_uuid, &b.set_uuid);
    }
};

pub const Header = extern struct {
    magic: [8]u8,
    version: u32,
    /// 0 = the one implicit segment at `base`.
    n_segments: u32,
    base: u64,
    pages: u64,
    grain: u64,
    n_slots: u32,
    path_len: u32,
    slots_off: u64,
    mirror_off: u64,
    volume_uuid: [16]u8,
    set_uuid: [16]u8,
    /// `RING_FIXED` or `RING_JOINED`.
    ring: u32,
    reserved: u32,
    segments: [journal.RING_SEGMENTS_MAX]Segment,
    path: [PATH_CAP]u8,

    pub fn ringBytes(self: *const Header) u64 {
        return self.pages * journal.PAGE_LEN;
    }

    pub fn pathSlice(self: *const Header) []const u8 {
        return self.path[0..self.path_len];
    }

    pub fn identity(self: *const Header) Identity {
        return .{ .volume_uuid = self.volume_uuid, .set_uuid = self.set_uuid };
    }

    /// Device offset of logical page `page` (`RingLayout.pageOffset`).
    pub fn pageOffset(self: *const Header, page: u64) u64 {
        if (self.n_segments == 0) return self.base + page * journal.PAGE_LEN;
        var p = page;
        for (self.segments[0..self.n_segments]) |s| {
            if (p < s.pages) return s.start + p * journal.PAGE_LEN;
            p -= s.pages;
        }
        unreachable;
    }

    /// The ring offset of a device offset inside the ring; null outside it.
    pub fn ringOffsetOf(self: *const Header, dev_off: u64) ?u64 {
        if (self.n_segments == 0) {
            if (dev_off < self.base or dev_off >= self.base + self.ringBytes()) return null;
            return dev_off - self.base;
        }
        var page_base: u64 = 0;
        for (self.segments[0..self.n_segments]) |s| {
            const len = s.pages * journal.PAGE_LEN;
            if (dev_off >= s.start and dev_off < s.start + len) return page_base * journal.PAGE_LEN + (dev_off - s.start);
            page_base += s.pages;
        }
        return null;
    }
};

pub const SLOT_FREE: u32 = 0;
pub const SLOT_FILLING: u32 = 1;
pub const SLOT_WRITTEN: u32 = 2;

pub const Run = extern struct { ring_off: u64, len: u64 };

pub const Slot = extern struct {
    /// `SLOT_*`: free → filling (the claimer's) → written (release) → free.
    state: u32,
    n_runs: u32,
    seq_base: u64,
    /// The window's reservation start: the ring's POSITION domain, the one
    /// the ledger's `journal_tail_seq` is in (a record's stamp is
    /// `seq_base`, `position + seq_offset`).
    pos_base: u64,
    /// The window's covering runs: one, or two across the ring's wrap.
    runs: [2]Run,
};

comptime {
    std.debug.assert(@sizeOf(Slot) == 56);
    std.debug.assert(@sizeOf(Segment) == 16);
}

pub const Facts = struct {
    layout: journal.RingLayout,
    grain: u64,
    path: []const u8,
    n_slots: u32,
    identity: Identity,
    ring: u32,
};

/// What a tail still holds: its written slots, their lowest position and
/// their record-stamp span.
pub const Held = struct {
    windows: u32 = 0,
    pos_lo: u64 = std.math.maxInt(u64),
    seq_lo: u64 = std.math.maxInt(u64),
    seq_hi: u64 = 0,
};

pub const ImageError = error{ BadMagic, BadVersion, Short, PathTooLong };

pub fn slotsOffset() u64 {
    return std.mem.alignForward(u64, @sizeOf(Header), std.atomic.cache_line);
}

pub fn mirrorOffset(n_slots: u32) u64 {
    return std.mem.alignForward(u64, slotsOffset() + @as(u64, n_slots) * @sizeOf(Slot), MIRROR_ALIGN);
}

/// The whole image's length for `facts`.
pub fn imageLen(facts: Facts) u64 {
    return mirrorOffset(facts.n_slots) + facts.layout.ringBytes();
}

pub const Replay = struct {
    /// Slots written to the device, in seq order.
    windows: u32 = 0,
    bytes: u64 = 0,
    /// Slots skipped: free (landed) or still filling (never acked).
    skipped: u32 = 0,
    /// Written slots below the replay's floor (`Image.replay`).
    behind: u32 = 0,
};

/// The replay's device: one write of `bytes` at device offset `off`.
pub const Sink = struct {
    ctx: *anyopaque,
    write: *const fn (ctx: *anyopaque, off: u64, bytes: []const u8) anyerror!void,
};

/// The tail's image over a byte buffer (a memfd mapping, or a test's
/// page-aligned allocation). Every accessor reads the header's offsets.
pub const Image = struct {
    bytes: []align(MIRROR_ALIGN) u8,

    /// Write the header for `facts` over zeroed bytes of `imageLen(facts)`.
    pub fn format(bytes: []align(MIRROR_ALIGN) u8, facts: Facts) ImageError!Image {
        if (facts.path.len > PATH_CAP) return error.PathTooLong;
        if (bytes.len < imageLen(facts)) return error.Short;
        const h: *Header = @ptrCast(bytes.ptr);
        h.* = .{
            .magic = MAGIC,
            .version = VERSION,
            .n_segments = facts.layout.n_segments,
            .base = facts.layout.base,
            .pages = facts.layout.pages,
            .grain = facts.grain,
            .n_slots = facts.n_slots,
            .path_len = @intCast(facts.path.len),
            .slots_off = slotsOffset(),
            .mirror_off = mirrorOffset(facts.n_slots),
            .volume_uuid = facts.identity.volume_uuid,
            .set_uuid = facts.identity.set_uuid,
            .ring = facts.ring,
            .reserved = 0,
            .segments = undefined,
            .path = undefined,
        };
        for (&h.segments, 0..) |*s, i| s.* = .{ .start = facts.layout.segments[i].start, .pages = facts.layout.segments[i].pages };
        @memset(&h.path, 0);
        @memcpy(h.path[0..facts.path.len], facts.path);
        return .{ .bytes = bytes };
    }

    /// Validate a header another process wrote.
    pub fn open(bytes: []align(MIRROR_ALIGN) u8) ImageError!Image {
        if (bytes.len < @sizeOf(Header)) return error.Short;
        const h: *const Header = @ptrCast(bytes.ptr);
        if (!std.mem.eql(u8, &h.magic, &MAGIC)) return error.BadMagic;
        if (h.version != VERSION) return error.BadVersion;
        if (h.path_len > PATH_CAP or h.n_segments > journal.RING_SEGMENTS_MAX or (h.ring != RING_FIXED and h.ring != RING_JOINED)) return error.Short;
        if (h.slots_off != slotsOffset() or h.mirror_off != mirrorOffset(h.n_slots)) return error.Short;
        if (bytes.len < h.mirror_off + h.ringBytes()) return error.Short;
        return .{ .bytes = bytes };
    }

    pub fn header(self: Image) *Header {
        return @ptrCast(self.bytes.ptr);
    }

    pub fn slots(self: Image) []Slot {
        const h = self.header();
        const p: [*]Slot = @ptrCast(@alignCast(self.bytes.ptr + h.slots_off));
        return p[0..h.n_slots];
    }

    pub fn mirror(self: Image) []u8 {
        const h = self.header();
        return self.bytes[h.mirror_off..][0..h.ringBytes()];
    }

    /// Claim a slot for a window: copy `ops` into the mirror, record the
    /// covering runs, `seq_base` and the reservation's `pos_base`, publish
    /// `written`. Null when the table is full (the window is then acked at
    /// the landing) or an op lies outside the ring (a caller bug, counted
    /// by the caller).
    pub fn claim(self: Image, cursor: *std.atomic.Value(u32), seq_base: u64, pos_base: u64, ops: []const journal.WriteOp) ?u32 {
        const h = self.header();
        const table = self.slots();
        if (table.len == 0) return null;
        const ring_bytes = h.ringBytes();
        var lo: u64 = std.math.maxInt(u64);
        var hi: u64 = 0;
        var lo_high: u64 = std.math.maxInt(u64);
        var hi_low: u64 = 0;
        var any = false;
        for (ops) |op| {
            if (op.bytes.len == 0) continue;
            const r = h.ringOffsetOf(op.offset) orelse return null;
            if (r + op.bytes.len > ring_bytes) return null;
            any = true;
            lo = @min(lo, r);
            hi = @max(hi, r + op.bytes.len);
            if (r >= ring_bytes / 2) lo_high = @min(lo_high, r) else hi_low = @max(hi_low, r + op.bytes.len);
        }
        if (!any) return null;
        // The slot: a cursor walk with one CAS per probe (windows free in
        // nearly the order they claim, so the probe after the cursor is
        // free in the common case).
        const start = cursor.fetchAdd(1, .monotonic);
        var probe: u32 = 0;
        const idx: u32 = while (probe < table.len) : (probe += 1) {
            const i: u32 = @intCast((start + probe) % table.len);
            // acq_rel pairs with `release`'s release store: the slot's
            // previous contents are dead before this claimer writes.
            if (@cmpxchgStrong(u32, &table[i].state, SLOT_FREE, SLOT_FILLING, .acq_rel, .monotonic) == null) break i;
        } else return null;
        const s = &table[idx];
        s.seq_base = seq_base;
        s.pos_base = pos_base;
        // The wrap: ops in both halves whose wrapped cover (the high run to
        // the ring's end, the low run from 0) is shorter than the linear
        // one — a window is under half the ring (`conveyor.resolveCaps`
        // caps it at ring/16), so the shorter cover is the window's.
        const wrapped = lo_high != std.math.maxInt(u64) and hi_low != 0 and (ring_bytes - lo_high) + hi_low < hi - lo;
        if (!wrapped) {
            s.n_runs = 1;
            s.runs[0] = .{ .ring_off = lo, .len = hi - lo };
            s.runs[1] = .{ .ring_off = 0, .len = 0 };
        } else {
            s.n_runs = 2;
            s.runs[0] = .{ .ring_off = lo_high, .len = ring_bytes - lo_high };
            s.runs[1] = .{ .ring_off = 0, .len = hi_low };
        }
        const m = self.mirror();
        for (ops) |op| {
            if (op.bytes.len == 0) continue;
            const r = h.ringOffsetOf(op.offset).?;
            @memcpy(m[r..][0..op.bytes.len], op.bytes);
        }
        // release pairs with the replayer's acquire load of `state`: the
        // runs and the mirror bytes are complete before `written` shows.
        @atomicStore(u32, &s.state, SLOT_WRITTEN, .release);
        return idx;
    }

    /// The window landed (or died unanswered): the slot is free.
    pub fn release(self: Image, idx: u32) void {
        // release: the next claimer's acq_rel CAS sees this slot dead.
        @atomicStore(u32, &self.slots()[idx].state, SLOT_FREE, .release);
    }

    pub fn writtenCount(self: Image) u32 {
        var n: u32 = 0;
        for (self.slots()) |*s| if (@atomicLoad(u32, &s.state, .acquire) == SLOT_WRITTEN) {
            n += 1;
        };
        return n;
    }

    /// The written slots' summary (a dead writer's image: nothing moves).
    pub fn held(self: Image) Held {
        var out: Held = .{};
        for (self.slots()) |*s| {
            // acquire pairs with `claim`'s release store.
            if (@atomicLoad(u32, &s.state, .acquire) != SLOT_WRITTEN) continue;
            out.windows += 1;
            out.pos_lo = @min(out.pos_lo, s.pos_base);
            out.seq_lo = @min(out.seq_lo, s.seq_base);
            out.seq_hi = @max(out.seq_hi, s.seq_base);
        }
        return out;
    }

    /// A held tail is STALE when the ledger's checkpoint tail has passed
    /// every window it holds: the dead writer's checkpoint never passes an
    /// un-landed window (the registry's open entry holds it back), so a
    /// window behind it landed (its slot not yet freed at the death) or was
    /// written over by a writer that ran without the tail — either way its
    /// bytes are not the ring's to restore. A window at or past the tail is
    /// replayed; the ones behind are skipped (`replay`'s floor).
    pub fn staleAgainst(self: Image, ledger_tail: u64) bool {
        var written: u32 = 0;
        for (self.slots()) |*s| {
            // acquire pairs with `claim`'s release store.
            if (@atomicLoad(u32, &s.state, .acquire) != SLOT_WRITTEN) continue;
            if (s.pos_base >= ledger_tail) return false;
            written += 1;
        }
        return written != 0;
    }

    /// Write every `written` slot's runs to `sink` in `seq_base` order,
    /// each run split at the ring's segment boundaries. Idempotent: the
    /// bytes are the lane's own image, a landed slot is free and skipped.
    /// A slot whose reservation lies below `floor_pos` (the ledger's
    /// checkpoint tail: landed and checkpointed, its ring space reusable)
    /// is skipped and counted `behind`.
    pub fn replay(self: Image, sink: Sink, floor_pos: u64) anyerror!Replay {
        const h = self.header();
        const table = self.slots();
        var out: Replay = .{};
        // Selection in seq order without allocation: the smallest unvisited
        // `seq_base` each round (the table is the lane's in-flight bound —
        // a few thousand at most — and a replay runs once).
        var last_seq: u64 = 0;
        var first = true;
        while (true) {
            var best: ?usize = null;
            for (table, 0..) |*s, i| {
                // acquire pairs with `claim`'s release store.
                if (@atomicLoad(u32, &s.state, .acquire) != SLOT_WRITTEN) continue;
                if (!first and s.seq_base <= last_seq) continue;
                if (best == null or s.seq_base < table[best.?].seq_base) best = i;
            }
            const i = best orelse break;
            const s = &table[i];
            first = false;
            last_seq = s.seq_base;
            if (s.pos_base < floor_pos) {
                out.behind += 1;
                continue;
            }
            for (s.runs[0..@min(s.n_runs, 2)]) |run| {
                if (run.len == 0) continue;
                if (run.ring_off + run.len > h.ringBytes()) return error.Corrupt;
                try self.writeRun(h, sink, run);
                out.bytes += run.len;
            }
            out.windows += 1;
        }
        for (table) |*s| if (@atomicLoad(u32, &s.state, .acquire) != SLOT_WRITTEN) {
            out.skipped += 1;
        };
        return out;
    }

    /// One run from the mirror, page by page across segments (a run is
    /// contiguous in ring space; its pages may be on different extents).
    fn writeRun(self: Image, h: *const Header, sink: Sink, run: Run) anyerror!void {
        const m = self.mirror();
        var off = run.ring_off;
        const end = run.ring_off + run.len;
        while (off < end) {
            const page = off / journal.PAGE_LEN;
            const in_page = off % journal.PAGE_LEN;
            // Pages of one segment are contiguous on the device: the chunk
            // runs to the segment's end or the run's.
            var chunk_end = end;
            if (h.n_segments != 0) {
                var acc: u64 = 0;
                for (h.segments[0..h.n_segments]) |s| {
                    acc += s.pages;
                    if (page < acc) {
                        chunk_end = @min(end, acc * journal.PAGE_LEN);
                        break;
                    }
                }
            }
            const dev_off = h.pageOffset(page) + in_page;
            try sink.write(sink.ctx, dev_off, m[off..chunk_end]);
            off = chunk_end;
        }
    }
};

// ---------------------------------------------------------------------------
// The daemon's tail (Linux): the memfd, size-sealed; the fd the supervisor
// receives; the announce channel; the held copy a failed replay leaves.
// ---------------------------------------------------------------------------

/// The supervisor's socket in THIS process (`daemon.detach`'s daemon role
/// sets it; −1 = no supervisor: a foreground mount, `--daemon` alone). The
/// mount announces each writable volume's tail and guard fd over it.
pub var supervisor_sock: std.atomic.Value(i32) = .init(-1);

pub fn hasSupervisor() bool {
    return supervisor_sock.load(.acquire) >= 0;
}

/// Where a held tail lives (decision 0003 §BQ.2): the host's RAM runtime
/// directory. `/run` is a tmpfs on every systemd host, so the copy outlives
/// the supervisor (its SIGKILL included) and dies with the host, whose
/// power cut is §5.2's window. Root's `/run/squeezefs/journal-tails`,
/// another uid's `/run/user/<uid>/squeezefs/journal-tails`;
/// `SQUEEZEFS_TEST_JOURNAL_TAIL_DIR` moves it into a rig's work dir.
pub const HeldDir = struct {
    buf: [PATH_CAP]u8 = undefined,
    len: usize = 0,

    pub fn slice(self: *const HeldDir) []const u8 {
        return self.buf[0..self.len];
    }

    pub fn fromEnv(env: *const std.process.Environ.Map) HeldDir {
        var d: HeldDir = .{};
        if (knobs.parseStr(env.get("SQUEEZEFS_TEST_JOURNAL_TAIL_DIR"))) |raw| if (!std.mem.eql(u8, raw, "off")) {
            d.len = (std.fmt.bufPrint(&d.buf, "{s}", .{raw}) catch return runtime()).len;
            return d;
        };
        return runtime();
    }

    pub fn runtime() HeldDir {
        var d: HeldDir = .{};
        const uid: u32 = if (builtin.os.tag == .linux) std.os.linux.geteuid() else 0;
        const text = if (uid == 0)
            std.fmt.bufPrint(&d.buf, "/run/squeezefs/journal-tails", .{})
        else
            std.fmt.bufPrint(&d.buf, "/run/user/{d}/squeezefs/journal-tails", .{uid});
        d.len = (text catch unreachable).len;
        return d;
    }
};

/// The held copy of a volume's tail: `<dir>/<volume uuid, hex>.jtail` — one
/// per volume (a writer of the volume consumes it before it reads the ring,
/// so a second never meets the first).
pub fn heldPath(buf: *[PATH_CAP]u8, dir: []const u8, volume_uuid: [16]u8) ?[:0]const u8 {
    const hex = std.fmt.bytesToHex(volume_uuid, .lower);
    return std.fmt.bufPrintSentinel(buf, "{s}/{s}.jtail", .{ dir, &hex }, 0) catch null;
}

pub const linux_tail = if (builtin.os.tag == .linux) struct {
    const linux = std.os.linux;

    pub const Tail = struct {
        fd: linux.fd_t,
        image: Image,
        cursor: std.atomic.Value(u32) = .init(0),

        pub const CreateError = error{ MemfdFailed, TruncateFailed, MapFailed, SealFailed } || ImageError;

        /// `memfd_create` + `ftruncate` + `mmap(MAP_SHARED)` + the size
        /// seals (`F_SEAL_SHRINK | GROW | SEAL`: the supervisor maps a
        /// length that cannot change under it; writes stay the daemon's).
        pub fn create(facts: Facts) CreateError!Tail {
            const len: usize = @intCast(imageLen(facts));
            const rc = linux.memfd_create("sqz-jrnl-tail", linux.MFD.CLOEXEC | linux.MFD.ALLOW_SEALING);
            if (linux.errno(rc) != .SUCCESS) return error.MemfdFailed;
            const fd: linux.fd_t = @intCast(rc);
            errdefer _ = linux.close(fd);
            if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.TruncateFailed;
            const map_rc = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
            if (linux.errno(map_rc) != .SUCCESS) return error.MapFailed;
            const ptr: [*]align(MIRROR_ALIGN) u8 = @ptrFromInt(map_rc);
            errdefer _ = linux.munmap(@ptrCast(ptr), len);
            const seals: u32 = linux.F.SEAL_SHRINK | linux.F.SEAL_GROW | linux.F.SEAL_SEAL;
            if (linux.errno(linux.fcntl(fd, linux.F.ADD_SEALS, seals)) != .SUCCESS) return error.SealFailed;
            const image = try Image.format(ptr[0..len], facts);
            return .{ .fd = fd, .image = image };
        }

        pub fn deinit(self: *Tail) void {
            _ = linux.munmap(@ptrCast(self.image.bytes.ptr), self.image.bytes.len);
            _ = linux.close(self.fd);
        }

        pub fn claim(self: *Tail, seq_base: u64, pos_base: u64, ops: []const journal.WriteOp) ?u32 {
            return self.image.claim(&self.cursor, seq_base, pos_base, ops);
        }

        pub fn release(self: *Tail, idx: u32) void {
            self.image.release(idx);
        }
    };

    /// The device as the replay's sink: `pwrite` (buffered — the writer is
    /// dead or not yet started: nothing else writes the device).
    const DeviceSink = struct {
        fd: linux.fd_t,

        fn write(ctx: *anyopaque, off: u64, bytes: []const u8) anyerror!void {
            const self: *DeviceSink = @ptrCast(@alignCast(ctx));
            var done: usize = 0;
            while (done < bytes.len) {
                const rc = linux.pwrite(self.fd, bytes.ptr + done, bytes.len - done, @intCast(off + done));
                switch (linux.errno(rc)) {
                    .SUCCESS => {
                        if (rc == 0) return error.InputOutput;
                        done += @intCast(rc);
                    },
                    .INTR => continue,
                    else => return error.InputOutput,
                }
            }
        }
    };

    pub const ReplayFdError = error{ StatFailed, MapFailed, DeviceOpenFailed, DeviceWriteFailed, BarrierFailed, DeviceReadFailed, Foreign, Stale } || ImageError;

    pub const ReplayOptions = struct {
        /// The device to write (null: the header's path).
        device_path: ?[]const u8 = null,
        /// The volume the replayer opened (its superblock's): a header
        /// naming another is refused `Foreign`.
        expect: ?Identity = null,
        /// Read the device's superblock and refuse `Foreign` when it names
        /// another volume (the supervisor's retry: the path may now name
        /// another device).
        verify_device: bool = false,
        /// The ledger's checkpoint tail (the successor's): a tail past it
        /// is refused `Stale` (`Image.staleAgainst`).
        ledger_tail: ?u64 = null,
    };

    /// One replay of a tail image held by `fd` (the memfd, or a held
    /// file): map it read-only, judge it (`opts`), open the device, write
    /// every written slot in seq order, `fsync`. `path_out` receives the
    /// device path (for the log line).
    pub fn replayFd(fd: linux.fd_t, opts: ReplayOptions, path_out: *[PATH_CAP]u8, path_len: *usize) ReplayFdError!Replay {
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return error.StatFailed;
        const len: usize = @intCast(st.size);
        if (len < @sizeOf(Header)) return error.Short;
        const map_rc = linux.mmap(null, len, .{ .READ = true }, .{ .TYPE = .SHARED }, fd, 0);
        if (linux.errno(map_rc) != .SUCCESS) return error.MapFailed;
        const ptr: [*]align(MIRROR_ALIGN) u8 = @ptrFromInt(map_rc);
        defer _ = linux.munmap(@ptrCast(ptr), len);
        const image = try Image.open(ptr[0..len]);
        const h = image.header();
        const dev_path = opts.device_path orelse h.pathSlice();
        if (dev_path.len > PATH_CAP) return error.PathTooLong;
        @memcpy(path_out[0..dev_path.len], dev_path);
        path_len.* = dev_path.len;
        if (opts.expect) |want| if (!want.eql(h.identity())) return error.Foreign;
        if (opts.ledger_tail) |t| if (image.staleAgainst(t)) return error.Stale;
        var zbuf: [PATH_CAP + 1]u8 = undefined;
        @memcpy(zbuf[0..dev_path.len], dev_path);
        zbuf[dev_path.len] = 0;
        const dev_rc = linux.open(zbuf[0..dev_path.len :0], .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(dev_rc) != .SUCCESS) return error.DeviceOpenFailed;
        var sink: DeviceSink = .{ .fd = @intCast(dev_rc) };
        defer _ = linux.close(sink.fd);
        if (opts.verify_device) {
            var sector: [superblock.sector_len]u8 = undefined;
            const n = linux.pread(sink.fd, &sector, sector.len, 0);
            if (linux.errno(n) != .SUCCESS or n != sector.len) return error.DeviceReadFailed;
            const on_dev: Identity = switch (superblock.classifySector0(&sector, null) catch return error.Foreign) {
                .v5 => |sb| .{ .volume_uuid = sb.uuid, .set_uuid = sb.set_uuid },
                .blank, .legacy => return error.Foreign,
            };
            if (!on_dev.eql(h.identity())) return error.Foreign;
        }
        // The replay's page cache leaves with the replayer on every
        // outcome: a refused write still `fsync`s what landed in the cache
        // before it, so their writeback error is consumed on THIS descriptor
        // and no dirty page is left for the flusher — a later writeback
        // error is handed to the successor's first barrier as a stale EIO
        // (xfstests generic/753: the next mount's fdatasync failed on a
        // healthy table), a later success lands old windows under its
        // O_DIRECT ring. Then the clean pages go.
        defer _ = linux.fadvise(sink.fd, 0, 0, linux.POSIX_FADV.DONTNEED);
        const out = image.replay(.{ .ctx = &sink, .write = DeviceSink.write }, opts.ledger_tail orelse 0) catch {
            _ = linux.fsync(sink.fd);
            return error.DeviceWriteFailed;
        };
        if (linux.errno(linux.fsync(sink.fd)) != .SUCCESS) return error.BarrierFailed;
        return out;
    }

    pub const PersistError = error{ StatFailed, MapFailed, DirFailed, CreateFailed, WriteFailed, Exists, NothingHeld, PathTooLong } || ImageError;

    pub const Persisted = struct { held: Held, identity: Identity };

    /// Copy a dead writer's tail (`src_fd`) into its held file under `dir`
    /// (`heldPath`): the header and the slot table whole, each written
    /// slot's runs at their mirror offsets — a sparse file of the same
    /// image, so `replayFd` reads either. Written beside the final name and
    /// renamed onto it without replacing (one held copy per volume: an
    /// existing one is `Exists`, kept). `path_out` receives the file's path.
    pub fn persistHeld(src_fd: linux.fd_t, dir: []const u8, path_out: *[PATH_CAP]u8) PersistError!Persisted {
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(src_fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return error.StatFailed;
        const len: usize = @intCast(st.size);
        if (len < @sizeOf(Header)) return error.Short;
        const map_rc = linux.mmap(null, len, .{ .READ = true }, .{ .TYPE = .SHARED }, src_fd, 0);
        if (linux.errno(map_rc) != .SUCCESS) return error.MapFailed;
        const ptr: [*]align(MIRROR_ALIGN) u8 = @ptrFromInt(map_rc);
        defer _ = linux.munmap(@ptrCast(ptr), len);
        const image = try Image.open(ptr[0..len]);
        const h = image.header();
        const held = image.held();
        if (held.windows == 0) return error.NothingHeld;
        try makeDirs(dir);
        const final = heldPath(path_out, dir, h.volume_uuid) orelse return error.PathTooLong;
        var tmp_buf: [PATH_CAP]u8 = undefined;
        const tmp = std.fmt.bufPrintSentinel(&tmp_buf, "{s}.{d}.tmp", .{ final, linux.getpid() }, 0) catch return error.PathTooLong;
        const rc = linux.open(tmp, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.CreateFailed;
        const fd: linux.fd_t = @intCast(rc);
        var keep = false;
        defer if (!keep) {
            _ = linux.unlink(tmp);
        };
        defer _ = linux.close(fd);
        const total = h.mirror_off + h.ringBytes();
        if (linux.errno(linux.ftruncate(fd, @intCast(total))) != .SUCCESS) return error.WriteFailed;
        try writeAll(fd, image.bytes[0..h.mirror_off], 0);
        const m = image.mirror();
        for (image.slots()) |*s| {
            if (@atomicLoad(u32, &s.state, .acquire) != SLOT_WRITTEN) continue;
            for (s.runs[0..@min(s.n_runs, 2)]) |run| {
                if (run.len == 0) continue;
                if (run.ring_off + run.len > h.ringBytes()) return error.Short;
                try writeAll(fd, m[run.ring_off..][0..run.len], h.mirror_off + run.ring_off);
            }
        }
        if (linux.errno(linux.fsync(fd)) != .SUCCESS) return error.WriteFailed;
        switch (linux.errno(linux.renameat2(linux.AT.FDCWD, tmp, linux.AT.FDCWD, final, .{ .NOREPLACE = true }))) {
            .SUCCESS => {},
            .EXIST => return error.Exists,
            else => return error.WriteFailed,
        }
        keep = true;
        return .{ .held = held, .identity = h.identity() };
    }

    fn writeAll(fd: linux.fd_t, bytes: []const u8, off: u64) PersistError!void {
        var done: usize = 0;
        while (done < bytes.len) {
            const rc = linux.pwrite(fd, bytes.ptr + done, bytes.len - done, @intCast(off + done));
            switch (linux.errno(rc)) {
                .SUCCESS => {
                    if (rc == 0) return error.WriteFailed;
                    done += @intCast(rc);
                },
                .INTR => continue,
                else => return error.WriteFailed,
            }
        }
    }

    /// `mkdir -p` with mode 0700 (the copies are a volume's metadata).
    fn makeDirs(dir: []const u8) PersistError!void {
        var buf: [PATH_CAP + 1]u8 = undefined;
        if (dir.len == 0 or dir.len > PATH_CAP) return error.DirFailed;
        @memcpy(buf[0..dir.len], dir);
        var i: usize = 1;
        while (i <= dir.len) : (i += 1) {
            if (i != dir.len and buf[i] != '/') continue;
            const saved = buf[i];
            buf[i] = 0;
            const e = linux.errno(linux.mkdirat(linux.AT.FDCWD, buf[0..i :0], 0o700)); // owner-only: the copies are volumes' metadata
            buf[i] = saved;
            if (e != .SUCCESS and e != .EXIST) return error.DirFailed;
        }
    }

    pub const Retry = union(enum) {
        /// Replayed and barriered: the held file is gone.
        landed: Replay,
        /// A writer of the volume holds its lock: it replays the file
        /// before it reads the ring (`consumeHeld`).
        busy,
        /// The file is gone: a writer replayed it.
        gone,
        /// The device at the header's path names another volume (a
        /// reformat, a re-stack): the file stays for that volume's next
        /// writer; retrying here is over.
        foreign,
        /// Still refused by the device (or unreadable): retry later.
        failed: ReplayFdError,
    };

    /// The supervisor's retry of a held file (`file`, its own path): under
    /// the volume's writer lock taken NON-BLOCKING on a fresh descriptor
    /// of the header's device (a live writer holds it: it is that writer's
    /// to replay), the file re-checked present under the lock, the device
    /// read for its identity, the replay, and the file unlinked on its
    /// landing — before the lock drops.
    pub fn retryHeld(file: [:0]const u8, path_out: *[PATH_CAP]u8, path_len: *usize) Retry {
        const rc = linux.open(file, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        switch (linux.errno(rc)) {
            .SUCCESS => {},
            .NOENT => return .gone,
            else => return .{ .failed = error.StatFailed },
        }
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var hdr: Header = undefined;
        const n = linux.pread(fd, std.mem.asBytes(&hdr), @sizeOf(Header), 0);
        if (linux.errno(n) != .SUCCESS or n != @sizeOf(Header)) return .{ .failed = error.Short };
        if (!std.mem.eql(u8, &hdr.magic, &MAGIC) or hdr.version != VERSION or hdr.path_len > PATH_CAP) return .{ .failed = error.BadMagic };
        var zbuf: [PATH_CAP + 1]u8 = undefined;
        @memcpy(zbuf[0..hdr.path_len], hdr.path[0..hdr.path_len]);
        zbuf[hdr.path_len] = 0;
        const lock_rc = linux.open(zbuf[0..hdr.path_len :0], .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(lock_rc) != .SUCCESS) return .{ .failed = error.DeviceOpenFailed };
        const lock_fd: linux.fd_t = @intCast(lock_rc);
        defer _ = linux.close(lock_fd);
        switch (linux.errno(linux.flock(lock_fd, std.posix.LOCK.EX | std.posix.LOCK.NB))) {
            .SUCCESS => {},
            .AGAIN => return .busy,
            else => return .{ .failed = error.DeviceOpenFailed },
        }
        // Under the lock: a writer that replayed it unlinked it first.
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .NLINK = true }, &st)) != .SUCCESS) return .{ .failed = error.StatFailed };
        if (st.nlink == 0) return .gone;
        const r = replayFd(fd, .{ .verify_device = true }, path_out, path_len) catch |e| return switch (e) {
            error.Foreign => .foreign,
            else => .{ .failed = e },
        };
        _ = linux.unlink(file);
        return .{ .landed = r };
    }

    pub const Consumed = union(enum) {
        /// No held copy of this volume's tail.
        none,
        /// Replayed and barriered, the file removed.
        landed: struct { replay: Replay, held: Held },
        /// Refused: its first window is behind the ledger's checkpoint
        /// tail (`Image.staleAgainst`) — written over by a writer that ran
        /// without it; removed, never written.
        stale: Held,
        /// Refused: named for this volume, the header names another; left.
        foreign: Identity,
        /// The device refused the replay (or the file is unreadable): the
        /// caller refuses to open.
        failed: ReplayFdError,
    };

    /// The successor's consumption of its volume's held tail (`file`):
    /// called under this writer's lock, after the superblock and the ledger
    /// were read and before the ring is (`KvBackend.open`).
    pub fn consumeHeld(file: [:0]const u8, device_path: []const u8, identity: Identity, ledger_tail: u64) Consumed {
        const rc = linux.open(file, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        switch (linux.errno(rc)) {
            .SUCCESS => {},
            .NOENT => return .none,
            else => return .{ .failed = error.StatFailed },
        }
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var held: Held = .{};
        {
            var st: linux.Statx = undefined;
            if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return .{ .failed = error.StatFailed };
            const len: usize = @intCast(st.size);
            if (len < @sizeOf(Header)) return .{ .failed = error.Short };
            const map_rc = linux.mmap(null, len, .{ .READ = true }, .{ .TYPE = .SHARED }, fd, 0);
            if (linux.errno(map_rc) != .SUCCESS) return .{ .failed = error.MapFailed };
            const ptr: [*]align(MIRROR_ALIGN) u8 = @ptrFromInt(map_rc);
            defer _ = linux.munmap(@ptrCast(ptr), len);
            const image = Image.open(ptr[0..len]) catch |e| return .{ .failed = e };
            held = image.held();
        }
        var pbuf: [PATH_CAP]u8 = undefined;
        var plen: usize = 0;
        const r = replayFd(fd, .{ .device_path = device_path, .expect = identity, .ledger_tail = ledger_tail }, &pbuf, &plen) catch |e| switch (e) {
            error.Stale => {
                _ = linux.unlink(file);
                return .{ .stale = held };
            },
            error.Foreign => {
                var hdr: Header = undefined;
                const n = linux.pread(fd, std.mem.asBytes(&hdr), @sizeOf(Header), 0);
                if (linux.errno(n) != .SUCCESS or n != @sizeOf(Header)) return .{ .failed = error.Short };
                return .{ .foreign = hdr.identity() };
            },
            else => return .{ .failed = e },
        };
        _ = linux.unlink(file);
        return .{ .landed = .{ .replay = r, .held = held } };
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const MemDevice = struct {
    bytes: []u8,
    writes: usize = 0,

    fn write(ctx: *anyopaque, off: u64, bytes: []const u8) anyerror!void {
        const self: *MemDevice = @ptrCast(@alignCast(ctx));
        if (off + bytes.len > self.bytes.len) return error.OutOfRange;
        @memcpy(self.bytes[@intCast(off)..][0..bytes.len], bytes);
        self.writes += 1;
    }

    fn sink(self: *MemDevice) Sink {
        return .{ .ctx = self, .write = write };
    }
};

const test_identity: Identity = .{ .volume_uuid = "jtail-test-vol!!".*, .set_uuid = "jtail-test-set!!".* };

fn testFacts(layout: journal.RingLayout, n_slots: u32) Facts {
    return .{ .layout = layout, .grain = 512, .path = "/dev/sqz-test", .n_slots = n_slots, .identity = test_identity, .ring = RING_FIXED };
}

fn allocImage(facts: Facts) ![]align(MIRROR_ALIGN) u8 {
    const bytes = try testing.allocator.alignedAlloc(u8, .fromByteUnits(MIRROR_ALIGN), @intCast(imageLen(facts)));
    @memset(bytes, 0);
    return bytes;
}

test "codec: the header round-trips through format/open; a wrong magic, version or a short buffer is refused; offsets are page-aligned for the mirror" {
    const layout: journal.RingLayout = .{ .base = 1 << 20, .pages = 8 };
    const facts = testFacts(layout, 16);
    const bytes = try allocImage(facts);
    defer testing.allocator.free(bytes);
    _ = try Image.format(bytes, facts);
    const img = try Image.open(bytes);
    const h = img.header();
    try testing.expectEqual(@as(u64, 1 << 20), h.base);
    try testing.expectEqual(@as(u64, 8), h.pages);
    try testing.expectEqual(@as(u32, 16), h.n_slots);
    try testing.expectEqual(@as(u64, 512), h.grain);
    try testing.expectEqualStrings("/dev/sqz-test", h.pathSlice());
    try testing.expectEqual(@as(u64, 0), h.mirror_off % MIRROR_ALIGN);
    try testing.expectEqual(@as(usize, 16), img.slots().len);
    try testing.expectEqual(@as(usize, 8 * journal.PAGE_LEN), img.mirror().len);
    try testing.expectEqual(@as(?u64, 0), h.ringOffsetOf(1 << 20));
    try testing.expectEqual(@as(?u64, null), h.ringOffsetOf((1 << 20) - 1));
    try testing.expectEqual(@as(?u64, null), h.ringOffsetOf((1 << 20) + 8 * journal.PAGE_LEN));
    // Refusals.
    bytes[0] ^= 1;
    try testing.expectError(error.BadMagic, Image.open(bytes));
    bytes[0] ^= 1;
    h.version += 1;
    try testing.expectError(error.BadVersion, Image.open(bytes));
    h.version -= 1;
    try testing.expectError(error.Short, Image.open(bytes[0..@sizeOf(Header)]));
    try testing.expectError(error.Short, Image.open(@alignCast(bytes[0 .. bytes.len - 1])));
}

test "claim + replay: a claimed window's bytes reach the device at the ops' offsets, in seq order across slots; a released (landed) slot is skipped; a second replay rewrites the same bytes (idempotent); a filling slot is never replayed" {
    const layout: journal.RingLayout = .{ .base = 4096, .pages = 4 };
    const facts = testFacts(layout, 4);
    const bytes = try allocImage(facts);
    defer testing.allocator.free(bytes);
    const img = try Image.format(bytes, facts);
    var cursor: std.atomic.Value(u32) = .init(0);
    var dev_bytes: [5 * 4096]u8 = @splat(0);
    var dev: MemDevice = .{ .bytes = &dev_bytes };

    // Two windows out of seq order: seq 200 claimed first, seq 100 second.
    const a_bytes: [1024]u8 = @splat(0xAA);
    const b_bytes: [512]u8 = @splat(0xBB);
    const b_hdr: [24]u8 = @splat(0xB0);
    const sa = img.claim(&cursor, 200, 200, &.{.{ .offset = 4096 + 8192, .bytes = &a_bytes }}).?;
    const sb = img.claim(&cursor, 100, 100, &.{ .{ .offset = 4096 + 4096, .bytes = &b_hdr }, .{ .offset = 4096 + 4096 + 24, .bytes = &b_bytes } }).?;
    try testing.expect(sa != sb);
    try testing.expectEqual(@as(u32, 2), img.writtenCount());
    // The covering run of the unpadded window spans header + entry.
    try testing.expectEqual(@as(u32, 1), img.slots()[sb].n_runs);
    try testing.expectEqual(@as(u64, 4096), img.slots()[sb].runs[0].ring_off);
    try testing.expectEqual(@as(u64, 24 + 512), img.slots()[sb].runs[0].len);

    var order: std.ArrayList(u64) = .empty;
    defer order.deinit(testing.allocator);
    const Order = struct {
        inner: *MemDevice,
        seen: *std.ArrayList(u64),
        fn write(ctx: *anyopaque, off: u64, b: []const u8) anyerror!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            try self.seen.append(testing.allocator, off);
            try MemDevice.write(self.inner, off, b);
        }
    };
    var ord: Order = .{ .inner = &dev, .seen = &order };
    const r1 = try img.replay(.{ .ctx = &ord, .write = Order.write }, 0);
    try testing.expectEqual(@as(u32, 2), r1.windows);
    try testing.expectEqual(@as(u64, 1024 + 24 + 512), r1.bytes);
    try testing.expectEqual(@as(u32, 2), r1.skipped);
    // Seq order: 100 (at 8192) before 200 (at 12288).
    try testing.expectEqual(@as(usize, 2), order.items.len);
    try testing.expectEqual(@as(u64, 4096 + 4096), order.items[0]);
    try testing.expectEqual(@as(u64, 4096 + 8192), order.items[1]);
    try testing.expectEqualSlices(u8, &b_hdr, dev_bytes[8192..][0..24]);
    try testing.expectEqualSlices(u8, &b_bytes, dev_bytes[8192 + 24 ..][0..512]);
    try testing.expectEqualSlices(u8, &a_bytes, dev_bytes[12288..][0..1024]);
    try testing.expectEqual(@as(u8, 0), dev_bytes[8192 + 24 + 512]);

    // The landing of seq 100 frees its slot: only seq 200 replays; the
    // device image is unchanged by the rewrite.
    img.release(sb);
    const before = dev_bytes;
    const r2 = try img.replay(dev.sink(), 0);
    try testing.expectEqual(@as(u32, 1), r2.windows);
    try testing.expectEqual(@as(u64, 1024), r2.bytes);
    try testing.expectEqualSlices(u8, &before, &dev_bytes);

    // A slot mid-fill is not replayed.
    img.slots()[sb].state = SLOT_FILLING;
    img.slots()[sb].seq_base = 50;
    const r3 = try img.replay(dev.sink(), 0);
    try testing.expectEqual(@as(u32, 1), r3.windows);
    img.slots()[sb].state = SLOT_FREE;

    // Refusals at claim: an op outside the ring; a full table.
    try testing.expectEqual(@as(?u32, null), img.claim(&cursor, 300, 300, &.{.{ .offset = 0, .bytes = &b_bytes }}));
    try testing.expectEqual(@as(?u32, null), img.claim(&cursor, 300, 300, &.{}));
    var filled: [4]u32 = undefined;
    var n: usize = 0;
    while (img.claim(&cursor, 400 + n, 400 + n, &.{.{ .offset = 4096, .bytes = &b_bytes }})) |i| : (n += 1) filled[n] = i;
    try testing.expectEqual(@as(usize, 3), n);
    for (filled[0..n]) |i| img.release(i);
    img.release(sa);
    try testing.expectEqual(@as(u32, 0), img.writtenCount());
}

test "claim: a window across the ring's wrap records two runs (the high run to the ring's end, the low run from 0) and replays both; a segmented ring's run is split at the segment boundary" {
    // A wrapped window on a 4-page ring: 512 B at the last page's end and
    // 512 B at page 0's start.
    {
        const layout: journal.RingLayout = .{ .base = 0, .pages = 4 };
        const facts = testFacts(layout, 2);
        const bytes = try allocImage(facts);
        defer testing.allocator.free(bytes);
        const img = try Image.format(bytes, facts);
        var cursor: std.atomic.Value(u32) = .init(0);
        const hi: [512]u8 = @splat(0x11);
        const lo: [512]u8 = @splat(0x22);
        const s = img.claim(&cursor, 7, 7, &.{ .{ .offset = 4 * 4096 - 512, .bytes = &hi }, .{ .offset = 0, .bytes = &lo } }).?;
        try testing.expectEqual(@as(u32, 2), img.slots()[s].n_runs);
        try testing.expectEqual(@as(u64, 4 * 4096 - 512), img.slots()[s].runs[0].ring_off);
        try testing.expectEqual(@as(u64, 512), img.slots()[s].runs[0].len);
        try testing.expectEqual(@as(u64, 0), img.slots()[s].runs[1].ring_off);
        try testing.expectEqual(@as(u64, 512), img.slots()[s].runs[1].len);
        var dev_bytes: [4 * 4096]u8 = @splat(0);
        var dev: MemDevice = .{ .bytes = &dev_bytes };
        const r = try img.replay(dev.sink(), 0);
        try testing.expectEqual(@as(u32, 1), r.windows);
        try testing.expectEqual(@as(u64, 1024), r.bytes);
        try testing.expectEqualSlices(u8, &hi, dev_bytes[4 * 4096 - 512 ..]);
        try testing.expectEqualSlices(u8, &lo, dev_bytes[0..512]);
    }
    // Two segments of one page each at device offsets 64 KiB and 16 KiB: a
    // run covering both pages lands on both extents.
    {
        const layout = try journal.RingLayout.fromSegments(&.{ .{ .start = 65536, .len = 4096 }, .{ .start = 16384, .len = 4096 } });
        const facts = testFacts(layout, 2);
        const bytes = try allocImage(facts);
        defer testing.allocator.free(bytes);
        const img = try Image.format(bytes, facts);
        try testing.expectEqual(@as(?u64, 4096), img.header().ringOffsetOf(16384));
        try testing.expectEqual(@as(?u64, 100), img.header().ringOffsetOf(65536 + 100));
        var cursor: std.atomic.Value(u32) = .init(0);
        const p0: [4096]u8 = @splat(0x33);
        const p1: [1024]u8 = @splat(0x44);
        _ = img.claim(&cursor, 1, 1, &.{ .{ .offset = 65536, .bytes = &p0 }, .{ .offset = 16384, .bytes = &p1 } }).?;
        var dev_bytes: [65536 + 4096]u8 = @splat(0);
        var dev: MemDevice = .{ .bytes = &dev_bytes };
        const r = try img.replay(dev.sink(), 0);
        try testing.expectEqual(@as(u32, 1), r.windows);
        try testing.expectEqual(@as(u64, 4096 + 1024), r.bytes);
        try testing.expectEqual(@as(usize, 2), dev.writes);
        try testing.expectEqualSlices(u8, &p0, dev_bytes[65536..][0..4096]);
        try testing.expectEqualSlices(u8, &p1, dev_bytes[16384..][0..1024]);
    }
}

test "held: the written slots' summary (windows, lowest position, stamp span); a tail is stale only when the ledger's checkpoint tail passed EVERY window it holds; a replay above a floor skips the windows behind it and writes the rest" {
    const layout: journal.RingLayout = .{ .base = 4096, .pages = 4 };
    const facts = testFacts(layout, 4);
    const bytes = try allocImage(facts);
    defer testing.allocator.free(bytes);
    const img = try Image.format(bytes, facts);
    var cursor: std.atomic.Value(u32) = .init(0);
    try testing.expectEqual(@as(u32, 0), img.held().windows);
    try testing.expect(!img.staleAgainst(1 << 40));
    const a: [512]u8 = @splat(0xA1);
    const b: [512]u8 = @splat(0xB2);
    // Stamps differ from positions (a handed-over slot's offset).
    _ = img.claim(&cursor, 9000, 4096, &.{.{ .offset = 4096 + 4096, .bytes = &a }}).?;
    _ = img.claim(&cursor, 9600, 8192, &.{.{ .offset = 4096 + 8192, .bytes = &b }}).?;
    const h = img.held();
    try testing.expectEqual(@as(u32, 2), h.windows);
    try testing.expectEqual(@as(u64, 4096), h.pos_lo);
    try testing.expectEqual(@as(u64, 9000), h.seq_lo);
    try testing.expectEqual(@as(u64, 9600), h.seq_hi);
    try testing.expectEqual(Identity.eql(test_identity, img.header().identity()), true);
    // The dead writer's checkpoint tail at or below the first window: fresh.
    try testing.expect(!img.staleAgainst(0));
    try testing.expect(!img.staleAgainst(4096));
    // Past the first window only: fresh (the second is replayed).
    try testing.expect(!img.staleAgainst(8192));
    // Past both: stale.
    try testing.expect(img.staleAgainst(8193));
    var dev_bytes: [5 * 4096]u8 = @splat(0);
    var dev: MemDevice = .{ .bytes = &dev_bytes };
    const r = try img.replay(dev.sink(), 8192);
    try testing.expectEqual(@as(u32, 1), r.windows);
    try testing.expectEqual(@as(u32, 1), r.behind);
    try testing.expectEqualSlices(u8, &b, dev_bytes[4096 + 8192 ..][0..512]);
    try testing.expectEqual(@as(u8, 0), dev_bytes[4096 + 4096]);
}

test "linux: the memfd tail is size-sealed, claims through the mapping, and replayFd writes the written slots to the header's device and barriers" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "the memfd tail is Linux only");
    const linux = std.os.linux;
    var dev_path_buf: [128]u8 = undefined;
    const dev_path = try std.fmt.bufPrintSentinel(&dev_path_buf, "/build/tmp/sqz-jrnl-tail-{d}.img", .{linux.getpid()}, 0);
    _ = linux.mkdirat(linux.AT.FDCWD, "/build/tmp", 0o755);
    const dev_rc = linux.open(dev_path, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(dev_rc));
    const dev_fd: linux.fd_t = @intCast(dev_rc);
    defer _ = linux.close(dev_fd);
    defer _ = linux.unlink(dev_path);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.ftruncate(dev_fd, 8 * 4096)));

    const layout: journal.RingLayout = .{ .base = 4096, .pages = 4 };
    var tail = try linux_tail.Tail.create(.{ .layout = layout, .grain = 512, .path = dev_path, .n_slots = 8, .identity = test_identity, .ring = RING_FIXED });
    defer tail.deinit();
    // Sealed: growing or shrinking is refused.
    try testing.expectEqual(linux.E.PERM, linux.errno(linux.ftruncate(tail.fd, 4096)));
    const payload: [2048]u8 = @splat(0x5A);
    const slot = tail.claim(11, 11, &.{.{ .offset = 4096 + 4096, .bytes = &payload }}).?;
    var pbuf: [PATH_CAP]u8 = undefined;
    var plen: usize = 0;
    const r = try linux_tail.replayFd(tail.fd, .{}, &pbuf, &plen);
    try testing.expectEqual(@as(u32, 1), r.windows);
    try testing.expectEqual(@as(u64, 2048), r.bytes);
    try testing.expectEqualStrings(dev_path, pbuf[0..plen]);
    var got: [2048 + 1]u8 = undefined;
    const n = linux.pread(dev_fd, &got, got.len, 8192);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(n));
    try testing.expectEqual(got.len, n);
    try testing.expectEqualSlices(u8, &payload, got[0..2048]);
    try testing.expectEqual(@as(u8, 0), got[2048]);
    tail.release(slot);
    const r2 = try linux_tail.replayFd(tail.fd, .{}, &pbuf, &plen);
    try testing.expectEqual(@as(u32, 0), r2.windows);
}

/// A scratch device file and a held dir under /build/tmp, pid-tagged.
const HeldRig = if (builtin.os.tag == .linux) struct {
    const linux = std.os.linux;
    dev_buf: [128]u8 = undefined,
    dir_buf: [128]u8 = undefined,
    dev_path: [:0]const u8 = undefined,
    dir: [:0]const u8 = undefined,
    dev_fd: linux.fd_t = -1,

    fn init(self: *HeldRig, tag: []const u8) !void {
        _ = linux.mkdirat(linux.AT.FDCWD, "/build/tmp", 0o755);
        self.dev_path = try std.fmt.bufPrintSentinel(&self.dev_buf, "/build/tmp/sqz-jtail-{s}-{d}.img", .{ tag, linux.getpid() }, 0);
        self.dir = try std.fmt.bufPrintSentinel(&self.dir_buf, "/build/tmp/sqz-jtail-{s}-{d}.held/a/b", .{ tag, linux.getpid() }, 0);
        const rc = linux.open(self.dev_path, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
        self.dev_fd = @intCast(rc);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.ftruncate(self.dev_fd, 8 * 4096)));
    }

    fn deinit(self: *HeldRig) void {
        _ = linux.close(self.dev_fd);
        _ = linux.unlink(self.dev_path);
        var b: [160]u8 = undefined;
        const top = std.fmt.bufPrint(&b, "{s}", .{self.dir[0 .. self.dir.len - "/a/b".len]}) catch return;
        std.Io.Dir.cwd().deleteTree(testing.io, top) catch {};
    }

    fn readAt(self: *HeldRig, buf: []u8, off: u64) !void {
        const n = linux.pread(self.dev_fd, buf.ptr, buf.len, @intCast(off));
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(n));
        try testing.expectEqual(buf.len, n);
    }

    fn exists(path: [:0]const u8) bool {
        var st: linux.Statx = undefined;
        return linux.errno(linux.statx(linux.AT.FDCWD, path, 0, .{ .SIZE = true }, &st)) == .SUCCESS;
    }
} else struct {};

test "linux: a failed replay's tail is HELD — persistHeld writes a sparse copy of the written slots under the held dir (0700, renamed in, never over an existing copy); the successor's consumeHeld replays it to ITS device path under the identity and the ledger's tail, barriers and removes it; a foreign copy is refused and left; a stale one removed unwritten; no copy is `none`" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "the held tail is Linux only");
    const linux = std.os.linux;
    var rig: HeldRig = .{};
    try rig.init("held");
    defer rig.deinit();
    const layout: journal.RingLayout = .{ .base = 4096, .pages = 4 };
    // The header names a path that no longer exists: the successor writes
    // its own (a re-stacked device under another name).
    var tail = try linux_tail.Tail.create(.{ .layout = layout, .grain = 512, .path = "/nonexistent/sqz-old-name", .n_slots = 8, .identity = test_identity, .ring = RING_FIXED });
    defer tail.deinit();
    var pbuf: [PATH_CAP]u8 = undefined;
    // Nothing written: nothing to hold.
    try testing.expectError(error.NothingHeld, linux_tail.persistHeld(tail.fd, rig.dir, &pbuf));
    const w1: [1024]u8 = @splat(0x61);
    const w2: [512]u8 = @splat(0x62);
    _ = tail.claim(500, 4096, &.{.{ .offset = 4096 + 4096, .bytes = &w1 }}).?;
    const landed = tail.claim(400, 2048, &.{.{ .offset = 4096 + 2048, .bytes = &w2 }}).?;
    _ = tail.claim(600, 12288, &.{.{ .offset = 4096 + 12288, .bytes = &w2 }}).?;
    tail.release(landed);
    const p = try linux_tail.persistHeld(tail.fd, rig.dir, &pbuf);
    try testing.expectEqual(@as(u32, 2), p.held.windows);
    try testing.expectEqual(@as(u64, 4096), p.held.pos_lo);
    var hb: [PATH_CAP]u8 = undefined;
    const file = heldPath(&hb, rig.dir, test_identity.volume_uuid).?;
    try testing.expectEqualStrings(file, std.mem.sliceTo(&pbuf, 0)[0..file.len]);
    {
        var st: linux.Statx = undefined;
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, rig.dir, 0, .{ .MODE = true }, &st)));
        try testing.expectEqual(@as(u16, 0o700), st.mode & 0o777);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, file, 0, .{ .SIZE = true, .BLOCKS = true }, &st)));
        try testing.expectEqual(imageLen(.{ .layout = layout, .grain = 512, .path = "", .n_slots = 8, .identity = test_identity, .ring = RING_FIXED }), st.size);
    }
    // One copy per volume: a second persist is refused, the first kept.
    try testing.expectError(error.Exists, linux_tail.persistHeld(tail.fd, rig.dir, &pbuf));
    try testing.expect(HeldRig.exists(file));
    // Foreign: the successor's volume is another — refused, left in place.
    const other: Identity = .{ .volume_uuid = test_identity.volume_uuid, .set_uuid = "another-set-uuid".* };
    switch (linux_tail.consumeHeld(file, rig.dev_path, other, 0)) {
        .foreign => |id| try testing.expect(id.eql(test_identity)),
        else => return error.TestUnexpectedResult,
    }
    try testing.expect(HeldRig.exists(file));
    // The successor: its device path, its identity, the dead writer's
    // ledger tail below the first window — both windows land; the copy goes.
    switch (linux_tail.consumeHeld(file, rig.dev_path, test_identity, 4096)) {
        .landed => |l| {
            try testing.expectEqual(@as(u32, 2), l.replay.windows);
            try testing.expectEqual(@as(u32, 2), l.held.windows);
        },
        else => return error.TestUnexpectedResult,
    }
    try testing.expect(!HeldRig.exists(file));
    var got1: [1024]u8 = undefined;
    try rig.readAt(&got1, 4096 + 4096);
    try testing.expectEqualSlices(u8, &w1, &got1);
    var got2: [512]u8 = undefined;
    try rig.readAt(&got2, 4096 + 12288);
    try testing.expectEqualSlices(u8, &w2, &got2);
    // The released (landed) window was not held: its bytes were never written.
    try rig.readAt(&got2, 4096 + 2048);
    try testing.expectEqual(@as(u8, 0), got2[0]);
    // No copy: none.
    try testing.expect(linux_tail.consumeHeld(file, rig.dev_path, test_identity, 0) == .none);
    // Stale: the ledger's tail past every held window — removed, unwritten.
    const zero: [1024]u8 = @splat(0);
    try testing.expectEqual(@as(usize, zero.len), linux.pwrite(rig.dev_fd, &zero, zero.len, 4096 + 4096));
    _ = try linux_tail.persistHeld(tail.fd, rig.dir, &pbuf);
    switch (linux_tail.consumeHeld(file, rig.dev_path, test_identity, 12289)) {
        .stale => |h| try testing.expectEqual(@as(u32, 2), h.windows),
        else => return error.TestUnexpectedResult,
    }
    try testing.expect(!HeldRig.exists(file));
    try rig.readAt(&got1, 4096 + 4096);
    try testing.expectEqual(@as(u8, 0), got1[0]);
}

test "linux: the supervisor's retry of a held copy — `busy` while a writer holds the volume's lock (that writer replays it), `failed` while the device refuses its superblock read, `foreign` when the device names another volume (the copy stays), `landed` once the device carries the volume's superblock (the copy removed under the lock), `gone` after" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "the held tail is Linux only");
    const linux = std.os.linux;
    var rig: HeldRig = .{};
    try rig.init("retry");
    defer rig.deinit();
    const sb = superblock.testFixture();
    const identity: Identity = .{ .volume_uuid = sb.uuid, .set_uuid = sb.set_uuid };
    // The ring past the superblock's sector.
    const layout: journal.RingLayout = .{ .base = 4096, .pages = 4 };
    var tail = try linux_tail.Tail.create(.{ .layout = layout, .grain = 512, .path = rig.dev_path, .n_slots = 4, .identity = identity, .ring = RING_FIXED });
    defer tail.deinit();
    const w: [2048]u8 = @splat(0x77);
    _ = tail.claim(70, 8192, &.{.{ .offset = 4096 + 8192, .bytes = &w }}).?;
    var pbuf: [PATH_CAP]u8 = undefined;
    _ = try linux_tail.persistHeld(tail.fd, rig.dir, &pbuf);
    var hb: [PATH_CAP]u8 = undefined;
    const file = heldPath(&hb, rig.dir, identity.volume_uuid).?;
    var dbuf: [PATH_CAP]u8 = undefined;
    var dlen: usize = 0;
    // A blank device: no volume's superblock — foreign, the copy stays.
    try testing.expect(linux_tail.retryHeld(file, &dbuf, &dlen) == .foreign);
    try testing.expect(HeldRig.exists(file));
    // Another volume's superblock: foreign.
    var other = sb;
    other.uuid = "not-this-volume!".*;
    var sector: [superblock.sector_len]u8 = undefined;
    try other.encodeSector(1, &sector, null);
    try testing.expectEqual(@as(usize, sector.len), linux.pwrite(rig.dev_fd, &sector, sector.len, 0));
    try testing.expect(linux_tail.retryHeld(file, &dbuf, &dlen) == .foreign);
    // This volume's superblock, a writer holding the lock: busy.
    try sb.encodeSector(1, &sector, null);
    try testing.expectEqual(@as(usize, sector.len), linux.pwrite(rig.dev_fd, &sector, sector.len, 0));
    {
        const lock_rc = linux.open(rig.dev_path, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(lock_rc));
        const lock_fd: linux.fd_t = @intCast(lock_rc);
        defer _ = linux.close(lock_fd);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(lock_fd, std.posix.LOCK.EX | std.posix.LOCK.NB)));
        try testing.expect(linux_tail.retryHeld(file, &dbuf, &dlen) == .busy);
    }
    try testing.expect(HeldRig.exists(file));
    // The lock free: landed, the copy removed.
    switch (linux_tail.retryHeld(file, &dbuf, &dlen)) {
        .landed => |r| try testing.expectEqual(@as(u32, 1), r.windows),
        else => return error.TestUnexpectedResult,
    }
    try testing.expect(!HeldRig.exists(file));
    var got: [2048]u8 = undefined;
    try rig.readAt(&got, 4096 + 8192);
    try testing.expectEqualSlices(u8, &w, &got);
    try testing.expect(linux_tail.retryHeld(file, &dbuf, &dlen) == .gone);
}

test "the held dir: the seam names it; root's is /run/squeezefs/journal-tails; another uid's under /run/user/<uid>; the copy's name is the volume uuid in hex" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try env.put("SQUEEZEFS_TEST_JOURNAL_TAIL_DIR", "/build/tmp/x/tails");
    try testing.expectEqualStrings("/build/tmp/x/tails", HeldDir.fromEnv(&env).slice());
    try env.put("SQUEEZEFS_TEST_JOURNAL_TAIL_DIR", "off");
    const d = HeldDir.fromEnv(&env);
    const uid: u32 = if (builtin.os.tag == .linux) std.os.linux.geteuid() else 0;
    if (uid == 0) try testing.expectEqualStrings("/run/squeezefs/journal-tails", d.slice()) else {
        var b: [64]u8 = undefined;
        try testing.expectEqualStrings(try std.fmt.bufPrint(&b, "/run/user/{d}/squeezefs/journal-tails", .{uid}), d.slice());
    }
    var hb: [PATH_CAP]u8 = undefined;
    const uuid: [16]u8 = .{ 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff };
    try testing.expectEqualStrings("/d/00112233445566778899aabbccddeeff.jtail", heldPath(&hb, "/d", uuid).?);
}
