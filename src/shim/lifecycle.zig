//! Process lifecycle of the shim (spec `docs/porting/il_shim.md` §5, §7;
//! decision 0003 §F): ctor-free lazy init behind the first interposer call
//! (other objects' constructors call interposed symbols before `main`; a
//! constructor of ours could run before or after them — the lazy init
//! serves either order), the thread-local reentrancy guard, the atfork
//! handlers, the build identity, linked-mode detection, and the panic
//! handler.
//!
//! The no-panic law, the Zig reading (0002 ledger S4): Zig has no
//! unwinding, so "no panic escapes an interposer" is a property of the
//! BODIES — every bound a checked branch yielding the real call, no
//! `unreachable` in a handler path — and the root `panic` is the backstop
//! for a true bug: it poisons every session (so a concurrent interposer on
//! another thread falls through to the real call at once), writes one line
//! raw, and aborts. The `.so` is built ReleaseSafe so a bug trips the
//! handler in the gate instead of becoming UB in a host.
//!
//! Async-signal posture: `enter`/`leave` are a thread-local store; the
//! state's lazy init allocates (the env map) and may establish a session
//! (connect, mmap) — it runs from `open*`, which POSIX lists AS-safe, so an
//! `open` from a signal handler over a NON-interposed frame pays the
//! control plane there. Over an interposed frame on the same thread the
//! guard routes the handler's call to the real function.

const std = @import("std");
const linux = std.os.linux;
const Atomic = std.atomic.Value;
const root = @import("root");

const sys = @import("sys.zig");
const classify = @import("classify.zig");
const fd_table = @import("fd_table.zig");

/// The `.so` root exports the build identity under this name (the daemon's
/// `build_options.git_commit`, imported by the root alone); the daemon's
/// test binary has none and runs the shim in linked mode under a fixed
/// non-degenerate test identity.
pub const in_so: bool = @hasDecl(root, "squeezefs_il_build_commit");
pub const build_commit: []const u8 = if (in_so) root.squeezefs_il_build_commit else "0123456789ab";
pub const train: []const u8 = if (in_so) root.squeezefs_il_train else "dev";
/// One static identity string (`squeezefs-il train <train> commit <commit>`):
/// the pairing lines print it, the release act and the shim census read it
/// off the .so.
pub const identity: []const u8 = identity_prefix ++ train ++ identity_commit_sep ++ build_commit;
pub const identity_prefix = "squeezefs-il train ";
pub const identity_commit_sep = " commit ";

// ---------------------------------------------------------------------------
// The reentrancy guard.
// ---------------------------------------------------------------------------

threadlocal var reentry: bool = false;

/// False = a nested entry over an interposed frame on this thread (the
/// shim's own libc use resolving to its exports, or a signal handler): the
/// caller takes the real call.
pub fn enter() bool {
    if (reentry) return false;
    reentry = true;
    return true;
}

pub fn leave() void {
    reentry = false;
}

// ---------------------------------------------------------------------------
// The state: uninit → initializing → ready | failed.
// ---------------------------------------------------------------------------

const UNINIT: u8 = 0;
const INITIALIZING: u8 = 1;
const READY: u8 = 2;
const FAILED: u8 = 3;

var state_word: Atomic(u8) = .init(UNINIT);
var state_storage: classify.State = undefined;
var atfork_registered: Atomic(bool) = .init(false);

/// The state if it is ready — never initializes (the data ops' probe: an
/// fd cannot be bound before the first `open` built the state).
pub fn stateIfReady() ?*classify.State {
    return if (state_word.load(.acquire) == READY) &state_storage else null;
}

/// The state, built on first use (the `open` family, `squeezefs_il_bind`).
/// A concurrent builder is waited for with a bounded spin; a failed build
/// (the env map could not be allocated) stays failed: every op is real.
pub fn state() ?*classify.State {
    while (true) {
        switch (state_word.load(.acquire)) {
            READY => return &state_storage,
            FAILED => return null,
            INITIALIZING => std.atomic.spinLoopHint(),
            else => {
                if (state_word.cmpxchgStrong(UNINIT, INITIALIZING, .acq_rel, .acquire) != null) continue;
                classify.initState(&state_storage, std.heap.c_allocator, sys.mapAnon) catch {
                    state_word.store(FAILED, .release);
                    return null;
                };
                registerAtfork();
                state_word.store(READY, .release);
                return &state_storage;
            },
        }
    }
}

// ---------------------------------------------------------------------------
// fork.
// ---------------------------------------------------------------------------

fn flushOne(_: void, f: fd_table.Flush) void {
    sys.seekSet(f.fd, f.offset);
}

/// PREPARE (the parent, pre-fork): epoch bump + one flush per armed mirror,
/// so the child inherits true kernel offsets and the demoted parent resyncs
/// from them. AS-safe (atomics, bounded `tryLock`, raw `lseek`).
pub fn forkPrepare() void {
    const st = stateIfReady() orelse return;
    st.table.forkFlushAll({}, flushOne);
}

fn poisonChildOne(_: void, s: *classify.Session) void {
    s.poisonChild();
}

/// CHILD: flag-poison every session and `close(2)` the inherited socket
/// COPIES (never `shutdown`: the description is the parent's too), clear
/// every slot (stale fds route to passthrough) and empty the registry so
/// the child lazily establishes its OWN sessions. The inherited arena
/// mappings stay mapped (munmap is not AS-safe) and die with exec/exit.
pub fn forkChild() void {
    const st = stateIfReady() orelse return;
    st.eachSession({}, poisonChildOne);
    const dropped = st.table.boundCount();
    st.table.clearAllInChild();
    st.resetRegistryInChild();
    _ = classify.passthrough_ops[@backingInt(classify.Reason.fork_child)].fetchAdd(dropped, .monotonic);
    reentry = false;
}

fn prepareC() callconv(.c) void {
    forkPrepare();
}

fn childC() callconv(.c) void {
    forkChild();
}

fn registerAtfork() void {
    if (atfork_registered.swap(true, .acq_rel)) return;
    _ = sys.pthread_atfork(prepareC, null, childC);
}

// ---------------------------------------------------------------------------
// The panic handler.
// ---------------------------------------------------------------------------

fn poisonOne(_: void, s: *classify.Session) void {
    s.poison();
}

/// Poison every session so every other thread's interposer falls through
/// at once (a poisoned session answers `fallthrough` before touching the
/// ring), one raw line, abort.
pub fn shimPanic(msg: []const u8, _: ?usize) noreturn {
    @branchHint(.cold);
    if (stateIfReady()) |st| st.eachSession({}, poisonOne);
    sys.writeStderr("squeezefs-il: internal error (every session poisoned, aborting): ");
    sys.writeStderr(msg);
    sys.writeStderr("\n");
    std.process.abort();
}

// ---------------------------------------------------------------------------
// Linked mode (S12): purely diagnostic. "Preload" iff `LD_PRELOAD` names
// our basename AND the loader honoured it (`AT_SECURE == 0`); the line
// names the path the loader actually mapped us from (`dl_iterate_phdr`).
// ---------------------------------------------------------------------------

pub const SONAME = "libsqueezefs_il.so";
pub const BASENAME_PREFIX = "libsqueezefs_il";

pub const LoadMode = enum { preload, linked };

/// Does an `LD_PRELOAD` value (`:`/space separated) name the shim by
/// basename prefix? A versioned `.so.X` matches; a directory component
/// named like the shim does not.
pub fn loadModeFrom(ld_preload: ?[]const u8, secure: bool) LoadMode {
    if (secure) return .linked;
    const v = ld_preload orelse return .linked;
    var it = std.mem.tokenizeAny(u8, v, ": ");
    while (it.next()) |entry| {
        const base = if (std.mem.lastIndexOfScalar(u8, entry, '/')) |i| entry[i + 1 ..] else entry;
        if (std.mem.startsWith(u8, base, BASENAME_PREFIX)) return .preload;
    }
    return .linked;
}

const PathFind = struct {
    probe: usize,
    buf: [256]u8 = undefined,
    len: usize = 0,
};

fn phdrCallback(info: *sys.dl_phdr_info, _: usize, data: ?*anyopaque) callconv(.c) c_int {
    const pf: *PathFind = @ptrCast(@alignCast(data.?));
    var i: usize = 0;
    while (i < info.phnum) : (i += 1) {
        const ph = info.phdr[i];
        if (ph.type != .LOAD) continue;
        const lo = info.addr + ph.vaddr;
        const hi = lo + ph.memsz;
        if (pf.probe >= lo and pf.probe < hi) {
            const name = std.mem.span(info.name orelse "");
            pf.len = @min(name.len, pf.buf.len);
            @memcpy(pf.buf[0..pf.len], name[0..pf.len]);
            return 1;
        }
    }
    return 0;
}

/// The path the loader mapped THIS object from ("" when unknown — a
/// statically linked test binary).
pub fn ownPath(buf: *[256]u8) []const u8 {
    var pf: PathFind = .{ .probe = @intFromPtr(&ownPath) };
    _ = sys.dl_iterate_phdr(phdrCallback, &pf);
    @memcpy(buf[0..pf.len], pf.buf[0..pf.len]);
    return buf[0..pf.len];
}

var load_mode_announced: Atomic(bool) = .init(false);

/// On the first blob decode, ONCE: name the mode when it is not preload
/// (a linked app must still ship its `.so` with the daemon — KD-7 is
/// mode-independent).
pub fn announceLoadModeOnce() void {
    if (load_mode_announced.swap(true, .acq_rel)) return;
    const st = stateIfReady() orelse return;
    const secure = sys.getauxval(sys.AT_SECURE) != 0;
    if (loadModeFrom(st.env.get("LD_PRELOAD"), secure) == .preload) return;
    var pbuf: [256]u8 = undefined;
    const path = ownPath(&pbuf);
    var lbuf: [400]u8 = undefined;
    const s = std.fmt.bufPrint(&lbuf, "squeezefs-il: active via direct link ({s}), not LD_PRELOAD — same build pairing applies\n", .{if (path.len == 0) "linked" else path}) catch return;
    sys.writeStderr(s);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "load mode: absent/empty env is linked; the bare soname, an absolute path, a versioned name and colon/space lists are preload; foreign-only and near-miss names are linked; AT_SECURE forces linked" {
    try testing.expectEqual(LoadMode.linked, loadModeFrom(null, false));
    try testing.expectEqual(LoadMode.linked, loadModeFrom("", false));
    try testing.expectEqual(LoadMode.preload, loadModeFrom("libsqueezefs_il.so", false));
    try testing.expectEqual(LoadMode.preload, loadModeFrom("/opt/sqz/lib/libsqueezefs_il.so", false));
    try testing.expectEqual(LoadMode.preload, loadModeFrom("libsqueezefs_il.so.2", false));
    try testing.expectEqual(LoadMode.preload, loadModeFrom("libjemalloc.so:libsqueezefs_il.so", false));
    try testing.expectEqual(LoadMode.preload, loadModeFrom("libjemalloc.so /x/libsqueezefs_il.so", false));
    try testing.expectEqual(LoadMode.linked, loadModeFrom("libjemalloc.so:libasan.so", false));
    try testing.expectEqual(LoadMode.linked, loadModeFrom("/libsqueezefs_il/libfoo.so", false));
    try testing.expectEqual(LoadMode.linked, loadModeFrom("libsqueezefs.so", false));
    try testing.expectEqual(LoadMode.linked, loadModeFrom("libsqueezefs_il.so", true));
}

test "the build identity is never degenerate; the lazy state builds once, is ready afterwards, and the guard is per thread" {
    try testing.expect(!@import("../ipc/wire.zig").buildCommitDegenerate(build_commit));
    const st = state() orelse return error.StateFailed;
    try testing.expectEqual(st, stateIfReady().?);
    try testing.expect(st.sessions_per_mount >= 1 and st.sessions_per_mount <= 16);
    try testing.expect(enter());
    try testing.expect(!enter());
    leave();
    try testing.expect(enter());
    leave();
    var pbuf: [256]u8 = undefined;
    _ = ownPath(&pbuf);
}
