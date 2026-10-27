//! `RLIMIT_RTTIME` in a process that never asks for real-time scheduling.
//! The limit bounds the CPU time a real-time thread runs between blocking
//! calls; at the hard limit the kernel's CPU-timer check SIGKILLs the
//! process (`check_thread_timers`, from `posix_cpu_timers_work`). A CFS
//! thread is real-time too while the kernel BOOSTS it: preempted inside an
//! RCU read section past the boost delay (`CONFIG_RCU_BOOST`, prio 98), or
//! holding a PI lock a real-time task waits on. A desktop session hands its
//! terminals `RLIMIT_RTTIME` 0/0 (this venue's: quickshell → foot → every
//! shell and agent under it), and at 0 the first boosted tick is a SIGKILL
//! with no log line and no core: the gate's daemons and `umount` verbs died
//! that way under load, one row in ~25 of a parallel loop, on dev too
//! (`docs/evidence/rung-10-rttime-kill.md`).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const testing = std.testing;
const testkit = @import("../core/testkit.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/rt_limit is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// The hard `RLIMIT_RTTIME` this process runs under, in µs (null:
/// unlimited).
pub fn lift() ?u64 {
    var lim: linux.rlimit = undefined;
    if (linux.errno(linux.getrlimit(.RTTIME, &lim)) != .SUCCESS) return null;
    if (lim.max == linux.RLIM.INFINITY) return null;
    return lim.max;
}

test "every squeezefs process lifts RLIMIT_RTTIME: one started under 0/0 µs (a desktop session's terminals hand it down) runs unlimited, and one under a finite soft limit below an unlimited hard one raises the soft — a CFS thread the kernel boosts to real time (an RCU reader preempted past the boost delay) is SIGKILLed at its first real-time tick under 0 (red: the gate's daemon and `umount` verb died of SIGKILLs their own CPU-timer work sent)" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "raising a hard RLIMIT_RTTIME needs CAP_SYS_RESOURCE");
    const Arm = struct { cur: linux.rlim_t, max: linux.rlim_t };
    for ([_]Arm{ .{ .cur = 0, .max = 0 }, .{ .cur = 0, .max = linux.RLIM.INFINITY } }) |arm| {
        // A child: the test process's own limit stays as it was.
        const rc = linux.fork();
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
        if (rc == 0) {
            const start: linux.rlimit = .{ .cur = arm.cur, .max = arm.max };
            if (linux.errno(linux.setrlimit(.RTTIME, &start)) != .SUCCESS) linux.exit_group(2);
            const left = lift();
            var lim: linux.rlimit = undefined;
            if (linux.errno(linux.getrlimit(.RTTIME, &lim)) != .SUCCESS) linux.exit_group(3);
            const open = left == null and lim.cur == linux.RLIM.INFINITY and lim.max == linux.RLIM.INFINITY;
            linux.exit_group(if (open) 0 else 1);
        }
        var status: i32 = 0;
        while (linux.errno(linux.waitpid(@intCast(rc), &status, 0)) == .INTR) {}
        const st: u32 = @bitCast(status);
        try testing.expect(linux.W.IFEXITED(st));
        try testing.expectEqual(@as(u8, 0), linux.W.EXITSTATUS(st));
    }
}
