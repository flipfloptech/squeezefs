//! The external mount supervisor (charter §4 "Also ship: an external mount
//! supervisor (`--daemon --supervise`: stat a control inode every 5 s;
//! after 30 s dump task stacks, SIGABRT→SIGKILL, then write
//! `/sys/fs/fuse/connections/<minor>/abort` if `waiting > 0`)";
//! `docs/porting/fuse_abi_session.md` §9.1, Q10; decision 0003 §S).
//!
//! It runs in the daemon's PARENT process (the intermediate of the double
//! fork, `daemon.zig`), std-only, no executor: it must outlive a wedged
//! daemon whose every thread is parked. The pure state machine `State` is
//! portable and tested; `run` is the Linux loop over it.
//!
//! Two probes, one per state of the mount (`probeKind`):
//! - ATTACHED (the mount's entry, by its id, is in the mount table at the
//!   mountpoint): `stat(<mountpoint>/.config)` — the charter's control
//!   inode, a LOOKUP-only entry with TTL 0, so every probe REACHES the
//!   daemon as a request and proves the FUSE path answers (a root `stat`
//!   under the 1 s attr TTL would measure the dentry cache;
//!   `stats_cli.md` §7 Q19). `.config` and not `.stats` (decision 0003
//!   §CC.3): a lookup of either renders its document, and the probe is a
//!   liveness question, not a reader — `.config` is the daemon's smallest
//!   document (the set summary, no census), and a probe's render of
//!   `.stats` inside a reader's share window would hand that reader the
//!   probe's snapshot.
//! - NOT ATTACHED (the daemon's own `umount2` landed and its teardown
//!   runs; or `squeezefs umount --lazy` detached the mountpoint and the
//!   daemon serves the holders left on the detached superblock until the
//!   kernel ends the connection): there is no path to stat — a stat of
//!   the mountpoint would reach whatever sits under it — and a detached
//!   but referenced mount is a DEFINED state, not a dead daemon. The probe
//!   is the daemon's HEARTBEAT: its watchdog tick stamps a word on a page
//!   the two processes share (`Heartbeat`, mapped by the intermediate
//!   before the fork), and a stamp older than two ticks is a miss. The
//!   watchdog runs through the whole teardown, so a long dismount pass
//!   (many staged files promoting) is never mistaken for a wedge.
//! A stat that fails on a mount that left the table between the two
//! looks is the teardown in progress, judged by the heartbeat too.
//!
//! Constants with their reasons:
//! - `PROBE_INTERVAL_NS` 5 s: one daemon watchdog tick (`watchdog.TICK_NS`,
//!   tied in `mount.zig`), the slowest periodic cadence the daemon already
//!   runs; past the 1 s attr TTL.
//! - `HEARTBEAT_STALE_NS` 2 × the interval: one tick of slack for a tick
//!   that lands late under load — a stamp two ticks old is a miss.
//! - `UNRESPONSIVE_NS` 30 s: six missed probes = six ticks of the daemon's
//!   own 5 s watchdog, long enough that a cold-device checkpoint or a
//!   ring-admission park never trips it (both are bounded in seconds, and
//!   the watchdog logs them), short enough that a wedged mount is freed
//!   before an application's own I/O timeouts (the JuiceFS precedent the
//!   reference cites).
//! - `ABORT_GRACE_NS` 2 s between SIGABRT and SIGKILL: the lock plane's tick
//!   (`sync/tick.zig`) — the time the daemon's own abort handler needs to
//!   write a core with the wedge in it; a daemon that cannot even die in
//!   one tick is killed.
//! - The sysfs `abort` write is the LAST rung and conditional on
//!   `waiting > 0`: it is destructive (ECONNABORTED to every blocked
//!   caller) and only needed when the kernel still holds requests the dead
//!   daemon can never answer.
//!
//! The journal tails (charter §5.2 "The ack point"; decision 0003 §BB.2–
//! BB.3): the daemon sends each writable volume's tail memfd and writer
//! guard fd over a `SOCK_SEQPACKET` socketpair the intermediate created
//! before the fork. The loop parks in `poll` on that socket for the probe
//! interval, so a daemon's death (the kernel closes its end: `POLLHUP`) is
//! seen at once, not a probe later. At the death — reaped on its own, or at
//! the end of the ladder — the supervisor REPLAYS every tail (the un-landed
//! windows to the device, in seq order, then `fsync`) and only then closes
//! the guard fds: the `flock`'s open file description is shared, so the
//! dead daemon's claim holds until the device is complete, and a successor
//! waits it out (`guard.proc.flockExclusive`'s bound). A replay the device
//! REFUSES is held, never discarded (decision 0003 §BR): the tail's written
//! slots are copied to its held file (`journal_tail.persistHeld`) before
//! the guards drop, the volume's next writer replays that copy before it
//! reads its ring, and this process retries it every tick under a
//! non-blocking writer lock until it lands — then exits.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");
const tick_law = @import("../sync/tick.zig");
const clock = @import("../core/clock.zig");
const knobs = @import("../core/knobs.zig");
const mountinfo = @import("../core/mountinfo.zig");
const journal_tail = @import("../meta/journal_tail.zig");
const staging = @import("../data/staging.zig");

/// The supervisor's thread name: the observability contract's, and how a
/// process census tells it from the daemon it shares the argv with (it
/// outlives its daemon while it holds custody, childless —
/// `umount.daemonByProcess`).
pub const thread_comm = "sqz-supervise";

/// The control inode's name under the mountpoint (module doc).
pub const CONTROL_INODE_NAME = ".config";

pub const PROBE_INTERVAL_NS: u64 = 5 * std.time.ns_per_s;
pub const HEARTBEAT_STALE_NS: u64 = 2 * PROBE_INTERVAL_NS;
pub const UNRESPONSIVE_NS: u64 = 30 * std.time.ns_per_s;
pub const ABORT_GRACE_NS: u64 = tick_law.tick_ns;

pub const Policy = struct {
    probe_interval_ns: u64 = PROBE_INTERVAL_NS,
    /// A control-inode probe's answer window: one watchdog tick whatever
    /// the cadence (a seam's faster cadence must not read a slow stat on a
    /// loaded host as a miss).
    probe_timeout_ns: u64 = PROBE_INTERVAL_NS,
    unresponsive_after_ns: u64 = UNRESPONSIVE_NS,
    heartbeat_stale_ns: u64 = HEARTBEAT_STALE_NS,
    write_abort: bool = true,
    /// Where a failed replay's tail is held (`journal_tail.HeldDir`;
    /// empty = the host's runtime dir).
    held_dir: journal_tail.HeldDir = .{},

    /// The policy a `--supervise` mount runs: the constants, the probe
    /// cadence `SQUEEZEFS_TEST_SUPERVISOR_PROBE_MS` when a suite sets it,
    /// the held tails' directory the mount's successor will read.
    pub fn fromEnv(env: *const std.process.Environ.Map) Policy {
        const ms = knobs.intKnob("SQUEEZEFS_TEST_SUPERVISOR_PROBE_MS", u64, env);
        return .{ .probe_interval_ns = if (ms == 0) PROBE_INTERVAL_NS else ms * std.time.ns_per_ms, .held_dir = journal_tail.HeldDir.fromEnv(env) };
    }
};

pub const Verdict = union(enum) {
    healthy,
    /// Probes had been failing and one succeeded: re-armed.
    recovered,
    /// Failing, inside the window: `for_ns` so far.
    grace: u64,
    /// Past the window: escalate exactly once until a success re-arms.
    escalate: u64,
    /// Already escalated and still failing: nothing more to do.
    escalated,
};

/// The pure machine (`SupervisorState::observe`).
pub const State = struct {
    last_ok_ns: u64,
    failing: bool = false,
    escalated: bool = false,

    pub fn init(now_ns: u64) State {
        return .{ .last_ok_ns = now_ns };
    }

    pub fn observe(self: *State, now_ns: u64, ok: bool, policy: Policy) Verdict {
        if (ok) {
            const was_failing = self.failing;
            self.last_ok_ns = now_ns;
            self.failing = false;
            self.escalated = false;
            return if (was_failing) .recovered else .healthy;
        }
        self.failing = true;
        const for_ns = now_ns -| self.last_ok_ns;
        if (for_ns < policy.unresponsive_after_ns) return .{ .grace = for_ns };
        if (self.escalated) return .escalated;
        self.escalated = true;
        return .{ .escalate = for_ns };
    }
};

/// The page the daemon and its supervisor share: the daemon's watchdog
/// tick stamps `tick_ns` (release); the supervisor reads it (acquire).
/// `extern` so the layout is one word at offset 0 across the fork.
pub const Heartbeat = extern struct {
    tick_ns: u64 = 0,

    pub fn beat(self: *Heartbeat) void {
        // release pairs with `lastNs`'s acquire in the other process.
        @atomicStore(u64, &self.tick_ns, clock.monotonicNs(), .release);
    }

    pub fn lastNs(self: *const Heartbeat) u64 {
        return @atomicLoad(u64, &self.tick_ns, .acquire);
    }

    /// A stamp within `stale_after_ns` of `now_ns`; a page never stamped
    /// (0) is stale — the daemon's first tick is one interval after ready.
    pub fn fresh(self: *const Heartbeat, now_ns: u64, stale_after_ns: u64) bool {
        const last = self.lastNs();
        return last != 0 and now_ns -| last < stale_after_ns;
    }
};

comptime {
    std.debug.assert(@sizeOf(Heartbeat) == @sizeOf(u64));
}

pub const ProbeKind = enum {
    control_inode,
    heartbeat,

    pub fn text(self: ProbeKind) []const u8 {
        return switch (self) {
            .control_inode => "stat of the control inode",
            .heartbeat => "the heartbeat",
        };
    }
};

/// Which probe judges this round (module doc).
pub fn probeKind(attached: bool) ProbeKind {
    return if (attached) .control_inode else .heartbeat;
}

/// One frame on the tail socket: what the fd beside it is. The write
/// cache's three (charter §6.1 "Staged"): its memfd, its CLAIM socket, and
/// — no fd — the dismount's word that the ring is drained (discard it).
pub const TailFrameKind = enum(u8) { tail = 1, guard = 2, write_cache = 3, write_cache_claim = 4, write_cache_drained = 5 };

pub const TailFrame = struct {
    kind: TailFrameKind,
    /// The volume's index in the set (the log line).
    index: u16,

    pub const LEN = 1 + @sizeOf(u16);

    pub fn encode(self: TailFrame) [LEN]u8 {
        var b: [LEN]u8 = undefined;
        b[0] = @backingInt(self.kind);
        std.mem.writeInt(u16, b[1..3], self.index, .little);
        return b;
    }

    pub fn decode(b: []const u8) ?TailFrame {
        if (b.len != LEN) return null;
        const kind: TailFrameKind = switch (b[0]) {
            1 => .tail,
            2 => .guard,
            3 => .write_cache,
            4 => .write_cache_claim,
            5 => .write_cache_drained,
            else => return null,
        };
        return .{ .kind = kind, .index = std.mem.readInt(u16, b[1..3], .little) };
    }
};

/// `waiting` is a decimal count on one line.
pub fn parseWaiting(text: []const u8) ?u64 {
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (t.len == 0) return null;
    return std.fmt.parseInt(u64, t, 10) catch null;
}

pub const linux_loop = if (builtin.os.tag == .linux) struct {
    const linux = std.os.linux;
    const Io = std.Io;
    const log = @import("../core/log.zig");
    const thread_name = @import("../core/thread_name.zig");
    const completion = @import("../sync/completion.zig");
    const rendezvous = @import("../ipc/rendezvous_linux.zig");

    /// The fds the daemon sent: its journal tails, its writer guards, and
    /// the write cache's memfd and claim (charter §6.1 "Staged": after the
    /// daemon's death the ring is HELD for the member's next mount unless
    /// the dismount said it was drained).
    pub const Tails = struct {
        gpa: std.mem.Allocator,
        sock: ?linux.fd_t,
        /// Where a failed replay's tail is held (the policy's; empty = the
        /// host's runtime dir).
        held_dir: journal_tail.HeldDir = .{},
        tails: std.ArrayList(struct { index: u16, fd: linux.fd_t }) = .empty,
        guards: std.ArrayList(linux.fd_t) = .empty,
        /// Failed replays held as files (`journal_tail.persistHeld`): this
        /// supervisor retries each under a non-blocking writer lock; the
        /// volume's next writer replays what is left before it reads its
        /// ring (decision 0003 §BR).
        held: std.ArrayList(HeldCopy) = .empty,
        /// Failed replays that could not be held as files: the memfd kept
        /// and the writer guards with it (no successor may open the volume
        /// over them), retried here.
        held_memfds: std.ArrayList(struct { index: u16, fd: linux.fd_t, attempts: u32 = 0 }) = .empty,
        write_cache: ?linux.fd_t = null,
        write_cache_claim: ?linux.fd_t = null,
        write_cache_drained: bool = false,
        write_cache_listening: bool = false,
        /// The daemon's end closed (its death, or its exit): nothing more
        /// arrives; the queued frames were drained first.
        hup: bool = false,
        /// The retry cadence of a held tail: the policy's probe interval
        /// (one watchdog tick; a suite's seam may shorten it).
        retry_ns: u64 = PROBE_INTERVAL_NS,

        pub const HeldCopy = struct {
            index: u16,
            path: [journal_tail.PATH_CAP]u8,
            len: usize,
            attempts: u32 = 0,

            fn pathZ(self: *HeldCopy) [:0]const u8 {
                return self.path[0..self.len :0];
            }
        };

        pub fn deinit(self: *Tails) void {
            for (self.tails.items) |t| _ = linux.close(t.fd);
            self.tails.deinit(self.gpa);
            for (self.held_memfds.items) |t| _ = linux.close(t.fd);
            self.held_memfds.deinit(self.gpa);
            self.held.deinit(self.gpa);
            self.releaseGuards();
            self.guards.deinit(self.gpa);
            self.dropWriteCache();
            if (self.sock) |s| _ = linux.close(s);
            self.sock = null;
        }

        fn dropWriteCache(self: *Tails) void {
            if (self.write_cache) |fd| _ = linux.close(fd);
            if (self.write_cache_claim) |fd| _ = linux.close(fd);
            self.write_cache = null;
            self.write_cache_claim = null;
        }

        /// Park up to `timeout_ns` on the socket: frames are taken as they
        /// arrive; `POLLHUP` (the daemon's end closed) returns at once and
        /// is remembered. Without a socket: the plain sleep.
        pub fn waitTick(self: *Tails, timeout_ns: u64) void {
            const sock = self.sock orelse return sleepNs(timeout_ns);
            if (self.hup) return sleepNs(timeout_ns);
            var pfd = [_]linux.pollfd{.{ .fd = sock, .events = linux.POLL.IN, .revents = 0 }};
            const deadline = clock.monotonicNs() + timeout_ns;
            while (true) {
                const now = clock.monotonicNs();
                if (now >= deadline) return;
                const ms: i32 = @intCast(@min((deadline - now + std.time.ns_per_ms - 1) / std.time.ns_per_ms, std.math.maxInt(i32)));
                const rc = linux.poll(&pfd, 1, ms);
                switch (linux.errno(rc)) {
                    .SUCCESS => {},
                    .INTR => continue,
                    else => return sleepNs(deadline -| clock.monotonicNs()),
                }
                if (rc == 0) return;
                self.drain();
                if (self.hup) return;
            }
        }

        /// Take every queued frame (never blocks).
        pub fn drain(self: *Tails) void {
            const sock = self.sock orelse return;
            var buf: [64]u8 = undefined;
            while (true) {
                const got = rendezvous.tryRecvCtl(sock, &buf) catch |e| {
                    if (e == error.Eof) self.hup = true;
                    return;
                } orelse return;
                const frame = TailFrame.decode(buf[0..got.len]) orelse {
                    if (got.fd) |fd| _ = linux.close(fd);
                    log.warn("supervisor: an unreadable frame on the tail socket ({d} bytes) — dropped", .{got.len});
                    continue;
                };
                if (frame.kind == .write_cache_drained) {
                    if (got.fd) |fd| _ = linux.close(fd);
                    self.write_cache_drained = true;
                    continue;
                }
                const fd = got.fd orelse {
                    log.warn("supervisor: a tail frame without a descriptor — dropped", .{});
                    continue;
                };
                const kept = switch (frame.kind) {
                    .tail => if (self.tails.append(self.gpa, .{ .index = frame.index, .fd = fd })) true else |_| false,
                    .guard => if (self.guards.append(self.gpa, fd)) true else |_| false,
                    .write_cache => blk: {
                        if (self.write_cache) |old| _ = linux.close(old);
                        self.write_cache = fd;
                        break :blk true;
                    },
                    .write_cache_claim => blk: {
                        if (self.write_cache_claim) |old| _ = linux.close(old);
                        self.write_cache_claim = fd;
                        break :blk true;
                    },
                    .write_cache_drained => unreachable,
                };
                if (!kept) _ = linux.close(fd);
            }
        }

        /// The daemon is dead: every tail to the device, in order, then the
        /// guards released. A tail whose replay FAILS is held, never
        /// discarded (decision 0003 §BR): copied into its held file BEFORE
        /// the guards drop (so the volume's next writer, whose lock waits
        /// for them, finds it), or — when it cannot be — kept here with the
        /// guards. Idempotent; logged per tail.
        pub fn replayAndRelease(self: *Tails) void {
            self.drain();
            self.listenWriteCacheClaim();
            const dir = self.heldDir();
            for (self.tails.items) |t| {
                var path: [journal_tail.PATH_CAP]u8 = undefined;
                var path_len: usize = 0;
                if (journal_tail.linux_tail.replayFd(self.gpa, t.fd, &path, &path_len)) |d| {
                    log.info("supervisor: journal tail of volume {d} ({s}): {d} un-landed window(s), {d} bytes replayed and barriered ({d} slot(s) landed or empty, {d} behind the ledger's tail; {s})", .{ t.index, path[0..path_len], d.replay.windows, d.replay.bytes, d.replay.skipped, d.behind, if (d.judged) "judged against the device's chain" else "nothing held" });
                    if (d.past_hole != 0) log.err("supervisor: journal tail of volume {d} ({s}): {d} acked window(s) lay past a hole the tail does not hold (a window acked at its landing whose write failed and was rolled back after they were applied) — NOT replayed: applying them past a missing transaction would break the volume's consistency; that metadata is lost", .{ t.index, path[0..path_len], d.past_hole });
                    _ = linux.close(t.fd);
                } else |e| self.holdFailed(t.index, t.fd, path[0..path_len], e, dir.slice());
            }
            self.tails.clearRetainingCapacity();
            if (self.held_memfds.items.len != 0) {
                log.err("supervisor: writer guard KEPT on {d} volume(s): {d} journal tail(s) that failed to replay could not be held as files — no mount may open those volumes over them; retrying every {d} ms (kill this process, pid {d}, to give them up and lose that metadata)", .{ self.guards.items.len, self.held_memfds.items.len, self.retry_ns / std.time.ns_per_ms, linux.getpid() });
                return;
            }
            if (self.guards.items.len != 0) log.info("supervisor: writer guard released on {d} volume(s) after the replay", .{self.guards.items.len});
            self.releaseGuards();
        }

        /// A failed replay of `fd` (volume `index`, its device `dev`): held.
        fn holdFailed(self: *Tails, index: u16, fd: linux.fd_t, dev: []const u8, e: anyerror, dir: []const u8) void {
            if (!isFixedRing(fd)) {
                // A joined ring's successor is the MANAGER's recovery of the
                // region (another process, often another node), which reads
                // the ring off the device at `T_owner`; the ring has no lock
                // of its own to hold, and the manager may re-lease it — a
                // copy held here reaches neither (decision 0003 BS.16).
                log.err("supervisor: journal tail of volume {d} ({s}): replay FAILED ({s}) — a JOINED ring's tail is not held (its successor is the manager's recovery off the device): acked metadata of the flush window may be lost; fsck the volume before the next mount", .{ index, dev, @errorName(e) });
                _ = linux.close(fd);
                return;
            }
            var hp: [journal_tail.PATH_CAP]u8 = undefined;
            if (journal_tail.linux_tail.persistHeld(fd, dir, &hp)) |p| {
                const copy_path = std.mem.sliceTo(&hp, 0);
                log.err("supervisor: journal tail of volume {d} ({s}): replay FAILED ({s}) — HELD: {d} acked window(s) (record stamps {d}..{d}) copied to {s}; the volume's next mount replays it before it admits writes (and refuses to open while it cannot), and this supervisor retries every {d} ms until the device answers", .{ index, dev, @errorName(e), p.held.windows, p.held.seq_lo, p.held.seq_hi, copy_path, self.retry_ns / std.time.ns_per_ms });
                var copy: HeldCopy = .{ .index = index, .path = undefined, .len = copy_path.len };
                @memcpy(copy.path[0..copy_path.len], copy_path);
                copy.path[copy_path.len] = 0;
                if (self.held.append(self.gpa, copy)) {
                    _ = linux.close(fd);
                    return;
                } else |_| {}
                // Out of memory for the list: the copy stands for the next
                // mount; this supervisor's retries of it are given up.
                log.warn("supervisor: out of memory tracking {s} — no retries from here; the next mount replays it", .{copy_path});
                _ = linux.close(fd);
                return;
            } else |pe| switch (pe) {
                error.NothingHeld => {
                    log.info("supervisor: journal tail of volume {d} ({s}): replay failed ({s}) with no window held — nothing acked to keep", .{ index, dev, @errorName(e) });
                    _ = linux.close(fd);
                    return;
                },
                else => {
                    if (self.held_memfds.append(self.gpa, .{ .index = index, .fd = fd })) {
                        log.err("supervisor: journal tail of volume {d} ({s}): replay FAILED ({s}) and the held copy under {s} could not be written ({s}) — kept in this supervisor with the volume's writer guard", .{ index, dev, @errorName(e), dir, @errorName(pe) });
                    } else |_| {
                        log.err("supervisor: journal tail of volume {d} ({s}): replay FAILED ({s}), no held copy ({s}), out of memory — acked metadata of the flush window is lost", .{ index, dev, @errorName(e), @errorName(pe) });
                        _ = linux.close(fd);
                    }
                },
            }
        }

        fn isFixedRing(fd: linux.fd_t) bool {
            var hdr: journal_tail.Header = undefined;
            const n = linux.pread(fd, std.mem.asBytes(&hdr), @sizeOf(journal_tail.Header), 0);
            if (linux.errno(n) != .SUCCESS or n != @sizeOf(journal_tail.Header)) return true;
            return hdr.ring != journal_tail.RING_JOINED;
        }

        fn heldDir(self: *const Tails) journal_tail.HeldDir {
            return if (self.held_dir.len != 0) self.held_dir else journal_tail.HeldDir.runtime();
        }

        /// Anything still held for a successor or a retry.
        pub fn holding(self: *const Tails) bool {
            return self.held.items.len != 0 or self.held_memfds.items.len != 0 or self.write_cache != null;
        }

        /// One retry round over every held tail (decision 0003 §BR.4).
        /// Logged on the first failure of each and at every doubling of
        /// its attempts.
        pub fn retryHeld(self: *Tails) void {
            var i: usize = 0;
            while (i < self.held.items.len) {
                const c = &self.held.items[i];
                c.attempts += 1;
                switch (journal_tail.linux_tail.retryHeld(self.gpa, c.pathZ())) {
                    .landed => |j| {
                        log.info("supervisor: the held journal tail of volume {d} replayed on retry {d}: {d} window(s), {d} bytes, barriered ({d} behind the ledger's tail skipped); {s} removed", .{ c.index, c.attempts, j.replay.windows, j.replay.bytes, j.behind, c.pathZ() });
                        if (j.past_hole != 0) log.err("supervisor: the held journal tail of volume {d}: {d} acked window(s) lay past a hole the tail does not hold (a window that failed and was rolled back, or never claimed a slot) — NOT replayed: applying them past a missing transaction would break the volume's consistency; that metadata is lost", .{ c.index, j.past_hole });
                        _ = self.held.swapRemove(i);
                        continue;
                    },
                    .gone => {
                        log.info("supervisor: the held journal tail of volume {d} ({s}) was replayed by the volume's next mount", .{ c.index, c.pathZ() });
                        _ = self.held.swapRemove(i);
                        continue;
                    },
                    .foreign => {
                        log.err("supervisor: the device of volume {d}'s held journal tail now names another volume (reformatted or re-stacked) — retries end; {s} stays for that volume's next mount", .{ c.index, c.pathZ() });
                        _ = self.held.swapRemove(i);
                        continue;
                    },
                    .stale => {
                        log.err("supervisor: the held journal tail of volume {d} is STALE (the ledger's checkpoint tail passed every window: a writer ran without it) — removed unwritten; the acked metadata it held is lost", .{c.index});
                        _ = self.held.swapRemove(i);
                        continue;
                    },
                    .busy => {},
                    .failed => |e| if (std.math.isPowerOfTwo(c.attempts)) log.warn("supervisor: the held journal tail of volume {d} ({s}): retry {d} refused ({s}) — retrying every {d} ms", .{ c.index, c.pathZ(), c.attempts, @errorName(e), self.retry_ns / std.time.ns_per_ms }),
                }
                i += 1;
            }
            var k: usize = 0;
            while (k < self.held_memfds.items.len) {
                const t = &self.held_memfds.items[k];
                t.attempts += 1;
                // The guards are still ours: no writer can hold the volume.
                if (journal_tail.linux_tail.retryKept(self.gpa, t.fd)) |j| {
                    log.info("supervisor: the kept journal tail of volume {d} replayed on retry {d}: {d} window(s), {d} bytes, barriered ({d} behind the ledger's tail skipped)", .{ t.index, t.attempts, j.replay.windows, j.replay.bytes, j.behind });
                    if (j.past_hole != 0) log.err("supervisor: the kept journal tail of volume {d}: {d} acked window(s) lay past a hole the tail does not hold — NOT replayed (the volume's consistency); that metadata is lost", .{ t.index, j.past_hole });
                    _ = linux.close(t.fd);
                    _ = self.held_memfds.swapRemove(k);
                    continue;
                } else |e| if (std.math.isPowerOfTwo(t.attempts)) log.warn("supervisor: the kept journal tail of volume {d}: retry {d} refused ({s})", .{ t.index, t.attempts, @errorName(e) });
                k += 1;
            }
            if (self.held_memfds.items.len == 0 and self.guards.items.len != 0) {
                log.info("supervisor: writer guard released on {d} volume(s) after the kept tails replayed", .{self.guards.items.len});
                self.releaseGuards();
            }
        }

        fn releaseGuards(self: *Tails) void {
            for (self.guards.items) |g| _ = linux.close(g);
            self.guards.clearRetainingCapacity();
        }

        /// The daemon is dead: hold what it left until it lands or a
        /// successor takes it, then exit. The write cache (residents, not
        /// drained by a dismount): LISTEN on the claim the daemon bound —
        /// the name is still ours — and hand the memfd to the member's next
        /// mount (`staging.Ring.Ram.bind` connects, takes ONE frame, then
        /// binds the claim itself); a drained or empty ring is discarded at
        /// once. A failed replay's tails: retried every tick (`retryHeld`).
        /// The hold has no deadline: both are acked custody (the kill -9
        /// law), and the operator who wants them gone kills this process —
        /// every fsync'd byte is on the data volumes already, and a held
        /// tail's copy stays for the volume's next mount.
        pub fn holdForSuccessor(self: *Tails) void {
            self.prepareWriteCacheHold();
            if (!self.holding()) return;
            if (self.held.items.len != 0 or self.held_memfds.items.len != 0) log.warn("supervisor: holding {d} journal tail(s) that failed to replay (pid {d}) — retried every {d} ms; the volume's next mount replays them before it admits writes", .{ self.held.items.len + self.held_memfds.items.len, linux.getpid(), self.retry_ns / std.time.ns_per_ms });
            var next_retry = clock.monotonicNs() + self.retry_ns;
            while (self.holding()) {
                const now = clock.monotonicNs();
                if (now >= next_retry) {
                    self.retryHeld();
                    next_retry = clock.monotonicNs() + self.retry_ns;
                    continue;
                }
                const claim = self.write_cache_claim orelse {
                    sleepNs(next_retry - now);
                    continue;
                };
                var pfd = [_]linux.pollfd{.{ .fd = claim, .events = linux.POLL.IN, .revents = 0 }};
                const ms: i32 = @intCast(@min((next_retry - now + std.time.ns_per_ms - 1) / std.time.ns_per_ms, std.math.maxInt(i32)));
                const rc = linux.poll(&pfd, 1, ms);
                switch (linux.errno(rc)) {
                    .SUCCESS => if (rc != 0) self.acceptSuccessor(),
                    .INTR => {},
                    else => sleepNs(next_retry -| clock.monotonicNs()),
                }
            }
            log.info("supervisor: nothing left to hold; done", .{});
        }

        /// The write cache's half of the hold: a drained or empty ring (or
        /// one the claim cannot serve) discarded, else listened on and said.
        fn prepareWriteCacheHold(self: *Tails) void {
            const memfd = self.write_cache orelse return;
            if (self.write_cache_claim == null) {
                log.err("supervisor: the write cache's memfd arrived without its claim — {d} B of acked unsynced custody cannot be handed over; discarded", .{memfdLen(memfd)});
                self.dropWriteCache();
                return;
            }
            if (self.write_cache_drained or !staging.Ring.Ram.holdsData(memfd)) {
                self.dropWriteCache();
                return;
            }
            self.listenWriteCacheClaim();
            if (self.write_cache == null) return;
            log.warn("supervisor: holding the dead daemon's write cache ({d} MiB of memfd, residents unpromoted) for this member's next mount — mount the set at this mount point to recover them; kill this process (pid {d}) to discard them (every fsync'd byte is on the data volumes)", .{ memfdLen(memfd) >> 20, linux.getpid() });
        }

        /// A successor connected to the write cache's claim: one frame with
        /// the memfd; sent, the ring is the successor's.
        fn acceptSuccessor(self: *Tails) void {
            const memfd = self.write_cache orelse return;
            const claim = self.write_cache_claim orelse return;
            const rc = linux.accept4(claim, null, null, linux.SOCK.CLOEXEC);
            switch (linux.errno(rc)) {
                .SUCCESS => {},
                .INTR, .AGAIN, .CONNABORTED => return,
                else => {
                    log.err("supervisor: accept on the write cache's claim failed — discarding {d} B of unsynced custody", .{memfdLen(memfd)});
                    self.dropWriteCache();
                    return;
                },
            }
            const conn: linux.fd_t = @intCast(rc);
            const frame = (TailFrame{ .kind = .write_cache, .index = 0 }).encode();
            const sent = if (rendezvous.sendCtl(conn, &frame, memfd)) true else |_| false;
            _ = linux.close(conn);
            if (sent) {
                log.info("supervisor: the write cache handed to the successor mount", .{});
                self.dropWriteCache();
            }
        }

        /// `listen` on the claim the moment the daemon is known dead —
        /// BEFORE the tails' replay, which can take a while: a successor
        /// mounting meanwhile connects (its retries cover the gap) instead
        /// of reading a bound, silent name as a live owner. Idempotent.
        pub fn listenWriteCacheClaim(self: *Tails) void {
            const memfd = self.write_cache orelse return;
            const claim = self.write_cache_claim orelse return;
            if (self.write_cache_listening) return;
            if (linux.errno(linux.listen(claim, 1)) != .SUCCESS) {
                log.err("supervisor: cannot listen on the write cache's claim — {d} B of acked unsynced custody cannot be handed over; discarded", .{memfdLen(memfd)});
                self.dropWriteCache();
                return;
            }
            self.write_cache_listening = true;
        }

        fn memfdLen(fd: linux.fd_t) u64 {
            var st: linux.Statx = undefined;
            if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return 0;
            return st.size;
        }
    };

    pub const sysfs_connections = "/sys/fs/fuse/connections";

    /// Map the shared page (`MAP_SHARED|MAP_ANONYMOUS`): called by the
    /// intermediate BEFORE it forks the daemon, so both see one page.
    pub fn mapHeartbeat() ?*Heartbeat {
        const rc = linux.mmap(null, std.heap.pageSize(), .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED, .ANONYMOUS = true }, -1, 0);
        if (linux.errno(rc) != .SUCCESS) return null;
        const page: *Heartbeat = @ptrFromInt(rc);
        page.* = .{};
        return page;
    }

    /// A probe runs on its own thread so a wedged mount cannot wedge the
    /// supervisor: the cell is shared, the LAST party to touch it frees it
    /// (a timed-out probe thread is otherwise leaked by design — it is
    /// parked in the kernel on the mount).
    const Probe = struct {
        gpa: std.mem.Allocator,
        path: [:0]u8,
        done: completion.Completion(bool) = .{},
        /// Two parties; the one that drops it to 0 frees.
        refs: std.atomic.Value(u32) = .init(2),

        fn release(self: *Probe) void {
            // acq_rel: the freeing side sees the other side's last write.
            if (self.refs.fetchSub(1, .acq_rel) == 1) {
                self.gpa.free(self.path);
                self.gpa.destroy(self);
            }
        }

        fn main(self: *Probe) void {
            var st: linux.Statx = undefined;
            const rc = linux.statx(linux.AT.FDCWD, self.path.ptr, 0, linux.STATX.BASIC_STATS, &st);
            _ = self.done.complete(linux.errno(rc) == .SUCCESS);
            self.release();
        }
    };

    /// `stat(<mountpoint>/.config)` within `timeout_ns`; false on a timeout
    /// or an error.
    pub fn probe(gpa: std.mem.Allocator, mountpoint: [:0]const u8, timeout_ns: u64) bool {
        const path = std.fmt.allocPrintSentinel(gpa, "{s}/" ++ CONTROL_INODE_NAME, .{mountpoint}, 0) catch return false;
        const p = gpa.create(Probe) catch {
            gpa.free(path);
            return false;
        };
        p.* = .{ .gpa = gpa, .path = path };
        const t = std.Thread.spawn(.{}, Probe.main, .{p}) catch {
            gpa.destroy(p);
            return false;
        };
        t.detach();
        const verdict = p.done.waitTimeout(timeout_ns) orelse false;
        p.release();
        return verdict;
    }

    /// The mount's connection id at START (a wedged mount wedges stat too):
    /// `/sys/fs/fuse/connections/<id>` is keyed by the mount's `st_dev`
    /// MINOR, which `statx` reports directly.
    pub fn connectionId(path: [:0]const u8) ?u64 {
        var st: linux.Statx = undefined;
        const rc = linux.statx(linux.AT.FDCWD, path.ptr, 0, linux.STATX.BASIC_STATS, &st);
        if (linux.errno(rc) != .SUCCESS) return null;
        return st.dev_minor;
    }

    /// The mount's id in the table at START (null when the table does not
    /// name the mountpoint): what `attached` is judged against.
    pub fn mountIdAt(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8) ?u64 {
        const buf = gpa.alloc(u8, mountinfo.READ_BYTES) catch return null;
        defer gpa.free(buf);
        const e = mountinfo.entryAt(mountinfo.read(io, buf), mountpoint) orelse return null;
        return e.id;
    }

    /// Is the mount with `id` still attached at `mountpoint`? An unknown
    /// id (the table was unreadable at start) reads the path's presence.
    pub fn attached(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8, id: ?u64) bool {
        const buf = gpa.alloc(u8, mountinfo.READ_BYTES) catch return true;
        defer gpa.free(buf);
        const e = mountinfo.entryAt(mountinfo.read(io, buf), mountpoint) orelse return false;
        return if (id) |want| e.id == want else true;
    }

    fn sleepNs(ns: u64) void {
        const ts: linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
        _ = linux.nanosleep(&ts, null);
    }

    fn alive(pid: linux.pid_t) bool {
        return linux.errno(linux.kill(pid, @fromBackingInt(@intCast(0)))) == .SUCCESS;
    }

    /// Reap the daemon when it exited (it is our child); true once reaped.
    fn reap(pid: linux.pid_t, status: *i32) bool {
        const rc = linux.waitpid(pid, status, linux.W.NOHANG);
        return linux.errno(rc) == .SUCCESS and @as(linux.pid_t, @intCast(rc)) == pid;
    }

    /// `/proc/<pid>/status` + every task's `comm`, `wchan`, `stack` (root
    /// only; unreadable files are skipped silently) into the log.
    pub fn dumpDaemonState(io: Io, pid: linux.pid_t) void {
        var pb: [64]u8 = undefined;
        var buf: [16384]u8 = undefined;
        const status_path = std.fmt.bufPrint(&pb, "/proc/{d}/status", .{pid}) catch return;
        if (Io.Dir.cwd().readFile(io, status_path, &buf)) |text| {
            log.err("supervisor: {s}:\n{s}", .{ status_path, text });
        } else |_| {}
        const task_dir = std.fmt.bufPrint(&pb, "/proc/{d}/task", .{pid}) catch return;
        var dir = Io.Dir.cwd().openDir(io, task_dir, .{ .iterate = true }) catch return;
        defer dir.close(io);
        var it = dir.iterate();
        while (it.next(io) catch null) |ent| {
            var comm_buf: [64]u8 = undefined;
            var wchan_buf: [128]u8 = undefined;
            var stack_buf: [4096]u8 = undefined;
            var fp: [128]u8 = undefined;
            const comm = readTaskFile(io, &fp, task_dir, ent.name, "comm", &comm_buf);
            const wchan = readTaskFile(io, &fp, task_dir, ent.name, "wchan", &wchan_buf);
            const stack = readTaskFile(io, &fp, task_dir, ent.name, "stack", &stack_buf);
            log.err("supervisor: task {s} comm={s} wchan={s}\n{s}", .{ ent.name, std.mem.trim(u8, comm, "\n"), wchan, stack });
        }
    }

    fn readTaskFile(io: Io, fp: *[128]u8, task_dir: []const u8, tid: []const u8, name: []const u8, buf: []u8) []const u8 {
        const path = std.fmt.bufPrint(fp, "{s}/{s}/{s}", .{ task_dir, tid, name }) catch return "";
        return Io.Dir.cwd().readFile(io, path, buf) catch "";
    }

    fn readWaiting(io: Io, conn: u64) ?u64 {
        var pb: [96]u8 = undefined;
        var buf: [32]u8 = undefined;
        const path = std.fmt.bufPrint(&pb, "{s}/{d}/waiting", .{ sysfs_connections, conn }) catch return null;
        const text = Io.Dir.cwd().readFile(io, path, &buf) catch return null;
        return parseWaiting(text);
    }

    fn writeAbort(conn: u64) bool {
        var pb: [96:0]u8 = undefined;
        const path = std.fmt.bufPrintSentinel(&pb, "{s}/{d}/abort", .{ sysfs_connections, conn }, 0) catch return false;
        const fd_rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
        if (linux.errno(fd_rc) != .SUCCESS) return false;
        const fd: linux.fd_t = @intCast(fd_rc);
        defer _ = linux.close(fd);
        return linux.errno(linux.write(fd, "1", 1)) == .SUCCESS;
    }

    /// The charter's ladder, once: dump → SIGABRT → (grace) → SIGKILL →
    /// reap → `abort` iff `waiting > 0`.
    pub fn escalate(io: Io, pid: linux.pid_t, conn: ?u64, for_ns: u64, kind: ProbeKind, policy: Policy, tails: *Tails) void {
        log.err("supervisor: daemon {d} unresponsive for {d} s (no answer to {s}) — dumping state, then SIGABRT → SIGKILL", .{ pid, for_ns / std.time.ns_per_s, kind.text() });
        dumpDaemonState(io, pid);
        _ = linux.kill(pid, .ABRT);
        const deadline = clock.monotonicNs() + ABORT_GRACE_NS;
        var status: i32 = 0;
        var reaped = false;
        while (!reaped and clock.monotonicNs() < deadline) {
            reaped = reap(pid, &status);
            if (!reaped) sleepNs(10 * std.time.ns_per_ms);
        }
        if (!reaped) {
            _ = linux.kill(pid, .KILL);
            while (!reap(pid, &status)) sleepNs(10 * std.time.ns_per_ms);
        }
        log.err("supervisor: daemon {d} is dead (wait status 0x{x})", .{ pid, @as(u32, @bitCast(status)) });
        tails.replayAndRelease();
        const c = conn orelse {
            log.err("supervisor: the connection id was unknown at start — nothing to abort; `umount -l` the mountpoint by hand", .{});
            return;
        };
        const waiting = readWaiting(io, c) orelse 0;
        if (policy.write_abort and waiting > 0) {
            if (writeAbort(c)) {
                log.err("supervisor: wrote {s}/{d}/abort ({d} request(s) were waiting): blocked callers get ECONNABORTED; `squeezefs umount` the mountpoint and remount", .{ sysfs_connections, c, waiting });
            } else {
                log.err("supervisor: writing {s}/{d}/abort failed; `umount -l` the mountpoint by hand", .{ sysfs_connections, c });
            }
        } else {
            log.err("supervisor: connection {d} has {d} waiting request(s); no abort written — `squeezefs umount` the mountpoint and remount", .{ c, waiting });
        }
    }

    /// The loop (the intermediate process after `ready`): returns the exit
    /// code — 0 when the daemon exited on its own (a clean umount), 1 after
    /// an escalation. `heartbeat` is the shared page (null only when the
    /// map failed at the fork: then the heartbeat probe is always a miss,
    /// said so at start).
    pub fn run(gpa: std.mem.Allocator, io: Io, pid: linux.pid_t, mountpoint: [:0]const u8, policy: Policy, heartbeat: ?*const Heartbeat, tail_sock: ?linux.fd_t) u8 {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, thread_comm);
        var tails: Tails = .{ .gpa = gpa, .sock = tail_sock, .held_dir = policy.held_dir, .retry_ns = policy.probe_interval_ns };
        defer tails.deinit();
        if (tail_sock == null) log.warn("supervisor: no tail socket — a daemon crash has the flush-window exposure (no journal tail replay)", .{});
        const conn = connectionId(mountpoint);
        const mount_id = mountIdAt(gpa, io, mountpoint);
        if (conn) |c| log.info("supervisor: watching daemon {d} at {s} (fuse connection {d}, mount id {?d}; probe every {d} ms, escalate after {d} s)", .{ pid, mountpoint, c, mount_id, policy.probe_interval_ns / std.time.ns_per_ms, policy.unresponsive_after_ns / std.time.ns_per_s }) else log.warn("supervisor: stat({s}) failed at start — the connection id is unknown, the abort rung is disabled", .{mountpoint});
        if (heartbeat == null) log.warn("supervisor: no shared heartbeat page — a detached mount's liveness cannot be judged; the escalation fires {d} s after its detach", .{policy.unresponsive_after_ns / std.time.ns_per_s});
        var state = State.init(clock.monotonicNs());
        var escalated = false;
        var was_attached = true;
        while (true) {
            tails.waitTick(policy.probe_interval_ns);
            var status: i32 = 0;
            // The daemon's end of the tail socket closed: it is exiting or
            // dead — wait for it (a blocking reap of a process already past
            // its fd table) rather than a probe later.
            const reaped = if (tails.hup) blk: {
                while (!reap(pid, &status)) sleepNs(std.time.ns_per_ms);
                break :blk true;
            } else reap(pid, &status);
            if (reaped) {
                tails.replayAndRelease();
                log.info("supervisor: daemon {d} exited (wait status 0x{x}); done", .{ pid, @as(u32, @bitCast(status)) });
                tails.holdForSuccessor();
                return if (escalated) 1 else 0;
            }
            if (!alive(pid)) {
                tails.replayAndRelease();
                tails.holdForSuccessor();
                return if (escalated) 1 else 0;
            }
            var is_attached = attached(gpa, io, mountpoint, mount_id);
            var kind = probeKind(is_attached);
            var ok = switch (kind) {
                .control_inode => probe(gpa, mountpoint, policy.probe_timeout_ns),
                .heartbeat => if (heartbeat) |h| h.fresh(clock.monotonicNs(), policy.heartbeat_stale_ns) else false,
            };
            // A stat that failed on a mount that left the table meanwhile
            // is the teardown (or a lazy detach) in progress: the heartbeat
            // judges it.
            if (!ok and kind == .control_inode and !attached(gpa, io, mountpoint, mount_id)) {
                is_attached = false;
                kind = .heartbeat;
                ok = if (heartbeat) |h| h.fresh(clock.monotonicNs(), policy.heartbeat_stale_ns) else false;
            }
            if (was_attached and !is_attached) log.info("supervisor: {s} is no longer attached (the daemon's own unmount in progress, or a lazy detach serving its holders): liveness by {s} from here", .{ mountpoint, kind.text() });
            was_attached = is_attached;
            const now = clock.monotonicNs();
            switch (state.observe(now, ok, policy)) {
                .healthy, .escalated => {},
                .recovered => log.info("supervisor: daemon {d} answers again after a missed probe", .{pid}),
                // A miss inside the grace window says nothing: the escalation
                // lines ("unresponsive for N s", the SIGQUIT, the kill) carry the
                // verdict, and an orderly teardown has an instant where the
                // control inode fails while the mount is still in the table —
                // a probe landing there is no event (the lazy-umount row's law:
                // no verdict on an orderly cycle). The miss is counted.
                .grace => {},
                .escalate => |for_ns| {
                    escalated = true;
                    escalate(io, pid, conn, for_ns, kind, policy, &tails);
                    tails.holdForSuccessor();
                    return 1;
                },
            }
        }
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests: the pure machine.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "state machine: escalates exactly once after the threshold; a success re-arms; grace reports the age" {
    const p: Policy = .{};
    var s = State.init(1000);
    try testing.expectEqual(Verdict.healthy, s.observe(1000 + 5 * std.time.ns_per_s, true, p));
    try testing.expectEqual(Verdict{ .grace = 5 * std.time.ns_per_s }, s.observe(1000 + 10 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict{ .grace = 29 * std.time.ns_per_s }, s.observe(1000 + 34 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict{ .escalate = 30 * std.time.ns_per_s }, s.observe(1000 + 35 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict.escalated, s.observe(1000 + 40 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict.escalated, s.observe(1000 + 400 * std.time.ns_per_s, false, p));
    // A success re-arms: recovered, then the window counts from it.
    try testing.expectEqual(Verdict.recovered, s.observe(1000 + 401 * std.time.ns_per_s, true, p));
    try testing.expectEqual(Verdict.healthy, s.observe(1000 + 406 * std.time.ns_per_s, true, p));
    try testing.expectEqual(Verdict{ .grace = 5 * std.time.ns_per_s }, s.observe(1000 + 411 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict{ .escalate = 30 * std.time.ns_per_s }, s.observe(1000 + 436 * std.time.ns_per_s, false, p));
}

test "the policy from the env: the constants unset; the probe seam changes the cadence alone — the probe's answer window and the heartbeat's staleness stay the watchdog tick's" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const d = Policy.fromEnv(&env);
    try testing.expectEqual(PROBE_INTERVAL_NS, d.probe_interval_ns);
    try testing.expectEqual(PROBE_INTERVAL_NS, d.probe_timeout_ns);
    try env.put("SQUEEZEFS_TEST_SUPERVISOR_PROBE_MS", "250");
    const s = Policy.fromEnv(&env);
    try testing.expectEqual(250 * std.time.ns_per_ms, s.probe_interval_ns);
    try testing.expectEqual(PROBE_INTERVAL_NS, s.probe_timeout_ns);
    try testing.expectEqual(HEARTBEAT_STALE_NS, s.heartbeat_stale_ns);
    try testing.expectEqual(UNRESPONSIVE_NS, s.unresponsive_after_ns);
}

test "constants: the probe reaches the daemon past the 1 s attr TTL; six probes to escalation; a heartbeat is stale after two ticks; the abort grace is one lock-plane tick" {
    try testing.expect(PROBE_INTERVAL_NS > std.time.ns_per_s);
    try testing.expectEqual(@as(u64, 6), UNRESPONSIVE_NS / PROBE_INTERVAL_NS);
    try testing.expectEqual(2 * PROBE_INTERVAL_NS, HEARTBEAT_STALE_NS);
    try testing.expect(HEARTBEAT_STALE_NS < UNRESPONSIVE_NS);
    try testing.expectEqual(tick_law.tick_ns, ABORT_GRACE_NS);
}

test "the probe kind follows the mount's attachment: the control inode while attached, the heartbeat once detached" {
    try testing.expectEqual(ProbeKind.control_inode, probeKind(true));
    try testing.expectEqual(ProbeKind.heartbeat, probeKind(false));
}

test "heartbeat: a never-stamped page is stale; a beat is fresh for two ticks and stale after; the daemon's beat is readable through the page" {
    var h: Heartbeat = .{};
    try testing.expect(!h.fresh(100 * std.time.ns_per_s, HEARTBEAT_STALE_NS));
    @atomicStore(u64, &h.tick_ns, 50 * std.time.ns_per_s, .release);
    try testing.expect(h.fresh(50 * std.time.ns_per_s + HEARTBEAT_STALE_NS - 1, HEARTBEAT_STALE_NS));
    try testing.expect(!h.fresh(50 * std.time.ns_per_s + HEARTBEAT_STALE_NS, HEARTBEAT_STALE_NS));
    // A clock that reads earlier than the stamp (another process's view)
    // is fresh, never a wrap.
    try testing.expect(h.fresh(49 * std.time.ns_per_s, HEARTBEAT_STALE_NS));
    h.beat();
    try testing.expect(h.fresh(clock.monotonicNs(), HEARTBEAT_STALE_NS));
    try testing.expect(h.lastNs() != 0);
}

test "a detached mount under heartbeat: fresh beats keep the machine healthy through the teardown; a daemon whose beats stop is escalated on the same 30 s law" {
    const p: Policy = .{};
    var h: Heartbeat = .{};
    var s = State.init(0);
    // Beats every tick for a minute: healthy throughout.
    var t: u64 = PROBE_INTERVAL_NS;
    while (t <= 60 * std.time.ns_per_s) : (t += PROBE_INTERVAL_NS) {
        @atomicStore(u64, &h.tick_ns, t - 1, .release);
        try testing.expectEqual(Verdict.healthy, s.observe(t, h.fresh(t, p.heartbeat_stale_ns), p));
    }
    // The beats stop: stale after two ticks, escalated 30 s after the last OK.
    const last_ok = t - PROBE_INTERVAL_NS;
    var verdicts: usize = 0;
    while (t < last_ok + 60 * std.time.ns_per_s) : (t += PROBE_INTERVAL_NS) {
        const v = s.observe(t, h.fresh(t, p.heartbeat_stale_ns), p);
        switch (v) {
            .healthy => try testing.expect(t - last_ok < HEARTBEAT_STALE_NS),
            .grace => |for_ns| try testing.expect(for_ns < UNRESPONSIVE_NS),
            .escalate => |for_ns| {
                verdicts += 1;
                try testing.expect(for_ns >= UNRESPONSIVE_NS);
            },
            .escalated => {},
            .recovered => return error.TestUnexpectedResult,
        }
    }
    try testing.expectEqual(@as(usize, 1), verdicts);
}

test "waiting parses one decimal line" {
    try testing.expectEqual(@as(?u64, 3), parseWaiting("3\n"));
    try testing.expectEqual(@as(?u64, 0), parseWaiting("0"));
    try testing.expectEqual(@as(?u64, null), parseWaiting(""));
    try testing.expectEqual(@as(?u64, null), parseWaiting("x"));
}

fn waitingFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [256]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseWaiting(buf[0..len]);
}

test "fuzz: the supervisor's waiting-count parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, waitingFuzz, .{});
}

test "tail frame: encode/decode round-trips both kinds; a wrong length or kind is refused" {
    inline for (.{ TailFrameKind.tail, TailFrameKind.guard, TailFrameKind.write_cache, TailFrameKind.write_cache_claim, TailFrameKind.write_cache_drained }) |k| {
        const f: TailFrame = .{ .kind = k, .index = 513 };
        const d = TailFrame.decode(&f.encode()).?;
        try testing.expectEqual(k, d.kind);
        try testing.expectEqual(@as(u16, 513), d.index);
    }
    try testing.expectEqual(@as(?TailFrame, null), TailFrame.decode(&.{ 6, 0, 0 }));
    try testing.expectEqual(@as(?TailFrame, null), TailFrame.decode(&.{ 1, 0 }));
    try testing.expectEqual(@as(?TailFrame, null), TailFrame.decode(&.{}));
}

fn tailFrameFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [8]u8 = undefined;
    const len = smith.slice(&buf);
    _ = TailFrame.decode(buf[0..len]);
}

test "fuzz: TailFrame.decode is total on arbitrary bytes" {
    try fuzzkit.fuzz({}, tailFrameFuzz, .{});
}
