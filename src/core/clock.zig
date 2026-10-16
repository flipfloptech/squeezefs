//! Clocks for hot paths: one `clock_gettime` call, no `std.Io` handle.
//!
//! `monotonicNs` is the deadline and op-latency clock: a FINE read, which on a
//! host whose clocksource has no vDSO mode (HPET, acpi_pm, some VMs) is a
//! syscall plus a device read (~1 µs on kamrui's HPET). `phaseNs` is the
//! phase-histogram clock (spans on one thread, no syscall), and
//! `monotonicCoarseNs` the clock for ages judged in milliseconds or more.
//! `realtimeCoarseS` is the rate-limiter clock (one log line per site per
//! second): COARSE on Linux because a tripwire fire must not pay a vDSO
//! fine-clock read on top of being a bug; plain REALTIME elsewhere.

const std = @import("std");
const builtin = @import("builtin");
const posix = std.posix;

/// Fine clock reads this process made (test builds only): the per-op clock
/// budget's probe (`rung-10-clockreads.md`).
pub var fine_reads: std.atomic.Value(u64) = .init(0);

fn read(clock: posix.clockid_t) posix.timespec {
    if (builtin.is_test and !(builtin.os.tag == .linux and (clock == .REALTIME_COARSE or clock == .MONOTONIC_COARSE))) _ = fine_reads.fetchAdd(1, .monotonic);
    var ts: posix.timespec = undefined;
    // clock_gettime on a valid clock id cannot fail; a failure here would be
    // a kernel bug, and zeroes are a safe answer for both callers (a phase of
    // 0 ns, a rate limiter that logs).
    switch (posix.errno(posix.system.clock_gettime(clock, &ts))) {
        .SUCCESS => return ts,
        else => return .{ .sec = 0, .nsec = 0 },
    }
}

pub fn monotonicNs() u64 {
    const ts = read(.MONOTONIC);
    return @as(u64, @intCast(ts.sec)) * std.time.ns_per_s + @as(u64, @intCast(ts.nsec));
}

/// CLOCK_MONOTONIC on its last tick (the vDSO, never the clocksource): for
/// ages and stamps judged in milliseconds or more (the lease's 1 s rule, the
/// watchdog's stamps). It trails `monotonicNs` by under `coarseLagNs`, so a
/// stamp from one clock is never aged by the other.
pub fn monotonicCoarseNs() u64 {
    const clock: posix.clockid_t = if (builtin.os.tag == .linux) .MONOTONIC_COARSE else .MONOTONIC;
    const ts = read(clock);
    return @as(u64, @intCast(ts.sec)) * std.time.ns_per_s + @as(u64, @intCast(ts.nsec));
}

/// The coarse clock's lag bound behind `monotonicNs`, from its tick
/// (clock_getres of CLOCK_MONOTONIC_COARSE: the jiffy), read once. The
/// timekeeper accumulates whole tick intervals and the coarse time is the
/// accumulation's edge, refreshed each tick: it trails by under 2 ticks
/// (kamrui, HZ=1000: up to 2.01 ms, 18 % of reads past one tick); the third
/// tick absorbs a late tick interrupt.
pub fn coarseLagNs() u64 {
    const known = coarse_lag_ns.load(.monotonic);
    if (known != 0) return known;
    var ts: posix.timespec = undefined;
    const clock: posix.clockid_t = if (builtin.os.tag == .linux) .MONOTONIC_COARSE else .MONOTONIC;
    const ns: u64 = switch (posix.errno(posix.system.clock_getres(clock, &ts))) {
        .SUCCESS => @as(u64, @intCast(ts.sec)) * std.time.ns_per_s + @as(u64, @intCast(ts.nsec)),
        // No answer: assume HZ=100, the longest tick a Linux kernel runs.
        else => 10 * std.time.ns_per_ms,
    };
    const lag = COARSE_LAG_TICKS * @max(ns, 1);
    coarse_lag_ns.store(lag, .monotonic);
    return lag;
}
var coarse_lag_ns: std.atomic.Value(u64) = .init(0);
/// Accumulation (< 2 ticks) plus one late tick: `coarseLagNs`'s doc.
const COARSE_LAG_TICKS = 3;

/// `monotonicNs` when `deadline_ns` may be within `horizon_ns` of now, else
/// null (it is surely farther): a coarse stamp trails fine time by under
/// `coarseLagNs`, so a deadline past `coarse + lag + horizon` cannot be nearer. A
/// timer check pays the fine read only near a deadline.
pub fn fineNear(deadline_ns: u64, horizon_ns: u64) ?u64 {
    if (monotonicCoarseNs() +| coarseLagNs() +| horizon_ns < deadline_ns) return null;
    return monotonicNs();
}

/// The phase clock: `monotonicNs`'s timeline read off the CPU's counter
/// (x86_64 TSC when CPUID reports it invariant, aarch64 CNTVCT), scaled by
/// this process's own calibration against `monotonicNs` — never the kernel's
/// TSC verdict, which on kamrui is a firmware calibration skew. For the span
/// between two reads on ONE thread (a phase histogram's boundaries, a poll):
/// never a deadline, never compared with another thread's stamp or with
/// `monotonicNs` (it drifts from it by its rate error). Until the
/// calibration window (`CAL_WINDOW_NS` after the first read) closes, and on a
/// CPU without such a counter, it IS `monotonicNs`.
/// shortcut: a thread migrating mid-span between CPUs whose counters are not
/// synchronized skews that one span (clamped at 0 by the `@max` callers
/// take); the venues are single-socket.
pub fn phaseNs() u64 {
    if (cal.state.load(.acquire) == .ready) return cal.at(counter());
    return phaseCalibrating();
}

/// Whether `phaseNs` reads the counter: null while its calibration is open,
/// false on a CPU without a usable one.
pub fn phaseOnCounter() ?bool {
    return switch (cal.state.load(.acquire)) {
        .ready => true,
        .none => false,
        else => null,
    };
}

/// The calibration span: ~1 µs anchor read brackets (a syscall on HPET) are
/// 2e-5 of it, and only the process's first 100 ms of phases fall back to
/// `monotonicNs`.
const CAL_WINDOW_NS = 100 * std.time.ns_per_ms;
/// The anchors' two read brackets together must be ≤ 1/1000 of the span, so
/// the scale errs ≤ 0.1 % (the phase test allows 1 %).
const CAL_BRACKET_RATIO = 1000;
/// `Cal.mult`'s fraction bits (32.32 fixed point): sub-ppb resolution for
/// any counter rate, and a 64-bit integer part.
const MULT_FRAC_BITS = 32;

const Cal = struct {
    const State = enum(u8) { unanchored, anchoring, anchored, scaling, ready, none };
    state: std.atomic.Value(State) = .init(.unanchored),
    m0: u64 = 0,
    c0: u64 = 0,
    /// The anchor's read bracket, in counts.
    w0: u64 = 0,
    m1: u64 = 0,
    c1: u64 = 0,
    /// ns per count, 32.32 fixed point.
    mult: u64 = 0,

    fn at(self: *const Cal, c: u64) u64 {
        const d = c -| self.c1;
        return self.m1 + @as(u64, @intCast((@as(u128, d) * self.mult) >> MULT_FRAC_BITS));
    }
};
var cal: Cal = .{};

fn phaseCalibrating() u64 {
    switch (cal.state.load(.acquire)) {
        .unanchored => if (cal.state.cmpxchgStrong(.unanchored, .anchoring, .acquire, .monotonic) == null) {
            if (!counterUsable()) {
                cal.state.store(.none, .release);
                return monotonicNs();
            }
            const before = counter();
            cal.m0 = monotonicNs();
            const after = counter();
            cal.c0 = before + (after - before) / 2;
            cal.w0 = after - before;
            // release pairs with the `.anchored` acquire below: m0/c0 visible.
            cal.state.store(.anchored, .release);
            return cal.m0;
        },
        .anchored => if (monotonicCoarseNs() -| cal.m0 >= CAL_WINDOW_NS and cal.state.cmpxchgStrong(.anchored, .scaling, .acquire, .monotonic) == null) {
            const before = counter();
            const m1 = monotonicNs();
            const after = counter();
            const c1 = before + (after - before) / 2;
            // A read bracket the scheduler widened (a preemption or a
            // migration between the counter reads) would skew the scale:
            // retry on a later read; a counter that ran backwards is unusable.
            if (after >= before and c1 > cal.c0 and m1 > cal.m0 and (cal.w0 + (after - before)) * CAL_BRACKET_RATIO <= c1 - cal.c0) {
                cal.c1 = c1;
                cal.m1 = m1;
                cal.mult = @intCast(@min((@as(u128, m1 - cal.m0) << MULT_FRAC_BITS) / (c1 - cal.c0), std.math.maxInt(u64)));
                // release pairs with `phaseNs`'s acquire: the scale is visible.
                cal.state.store(.ready, .release);
            } else cal.state.store(if (c1 <= cal.c0) .none else .anchored, .release);
            return m1;
        },
        else => {},
    }
    return monotonicNs();
}

fn counterUsable() bool {
    if (builtin.os.tag != .linux) return false;
    return switch (builtin.cpu.arch) {
        .x86_64 => invariantTsc(),
        .aarch64 => true,
        else => false,
    };
}

/// CPUID leaf 0x8000_0000: EAX = the highest extended leaf.
const CPUID_EXT_MAX_LEAF = 0x8000_0000;
/// CPUID leaf 0x8000_0007 (advanced power management): EDX bit 8 = invariant TSC.
const CPUID_APM_LEAF = 0x8000_0007;

fn invariantTsc() bool {
    var eax: u32 = CPUID_EXT_MAX_LEAF;
    var ebx: u32 = undefined;
    var ecx: u32 = 0;
    var edx: u32 = undefined;
    asm volatile ("cpuid"
        : [a] "={eax}" (eax),
          [b] "={ebx}" (ebx),
          [c] "={ecx}" (ecx),
          [d] "={edx}" (edx),
        : [a_in] "{eax}" (eax),
          [c_in] "{ecx}" (ecx),
    );
    if (eax < CPUID_APM_LEAF) return false;
    eax = CPUID_APM_LEAF;
    ecx = 0;
    asm volatile ("cpuid"
        : [a] "={eax}" (eax),
          [b] "={ebx}" (ebx),
          [c] "={ecx}" (ecx),
          [d] "={edx}" (edx),
        : [a_in] "{eax}" (eax),
          [c_in] "{ecx}" (ecx),
    );
    // EDX bit 8: the TSC runs at a constant rate through P- and C-states.
    return edx & (1 << 8) != 0;
}

inline fn counter() u64 {
    switch (builtin.cpu.arch) {
        .x86_64 => {
            var lo: u32 = undefined;
            var hi: u32 = undefined;
            asm volatile ("rdtsc"
                : [lo] "={eax}" (lo),
                  [hi] "={edx}" (hi),
            );
            return @as(u64, hi) << 32 | lo; // rdtsc returns the count as EDX:EAX halves
        },
        .aarch64 => return asm volatile ("mrs %[r], cntvct_el0"
            : [r] "=r" (-> u64),
        ),
        else => return 0,
    }
}

pub fn realtimeCoarseS() u64 {
    const clock: posix.clockid_t = if (builtin.os.tag == .linux) .REALTIME_COARSE else .REALTIME;
    const ts = read(clock);
    return if (ts.sec < 0) 0 else @intCast(ts.sec);
}

/// The i64-ns realtime stamp a metadata mutation carries (atime/mtime/
/// ctime): COARSE on Linux because the kernel authors its own cmtime from
/// the same coarse clock — a fine daemon clock could LEAD the kernel's
/// writeback-cache stamp by a tick and invert cross-inode ctime order
/// (fstests generic/423).
pub fn realtimeCoarseNs() i64 {
    const clock: posix.clockid_t = if (builtin.os.tag == .linux) .REALTIME_COARSE else .REALTIME;
    const ts = read(clock);
    return @as(i64, @intCast(ts.sec)) *| std.time.ns_per_s +| @as(i64, @intCast(ts.nsec));
}

test "the phase clock runs on monotonic's timeline: after its calibration window a span read on it matches the fine clock's within 1 %, and it never goes backwards; coarse monotonic trails fine by at most its lag bound" {
    // The calibration closes on the first read past its window.
    const first = phaseNs();
    while (monotonicNs() -| first < CAL_WINDOW_NS + 10 * std.time.ns_per_ms) std.atomic.spinLoopHint();
    _ = phaseNs();
    // Bracketed both ways, so a preemption between two reads only widens the
    // outer span and narrows the inner one.
    const outer0 = phaseNs();
    const m0 = monotonicNs();
    const inner0 = phaseNs();
    while (monotonicNs() -| m0 < 20 * std.time.ns_per_ms) std.atomic.spinLoopHint();
    const inner1 = phaseNs();
    const m1 = monotonicNs();
    const outer1 = phaseNs();
    try std.testing.expect(inner1 >= inner0 and outer1 >= inner1);
    const fine = m1 - m0;
    try std.testing.expect(inner1 - inner0 <= fine + fine / 100);
    try std.testing.expect(outer1 - outer0 + fine / 100 >= fine);
    const c = monotonicCoarseNs();
    const f = monotonicNs();
    try std.testing.expect(c <= f and f - c <= coarseLagNs() + 50 * std.time.ns_per_ms);
}

test "fineNear reads the fine clock only within the coarse clock's lag bound plus its horizon of the deadline" {
    const now = monotonicNs();
    const before = fine_reads.load(.monotonic);
    try std.testing.expectEqual(@as(?u64, null), fineNear(now + std.time.ns_per_s, coarseLagNs()));
    try std.testing.expect(fine_reads.load(.monotonic) == before or builtin.os.tag != .linux);
    try std.testing.expect(fineNear(now, 0).? >= now);
    try std.testing.expect(fineNear(now + std.time.ns_per_s, std.time.ns_per_s) != null);
}

test "monotonic never goes backwards; realtime is after 2020" {
    const a = monotonicNs();
    const b = monotonicNs();
    try std.testing.expect(b >= a);
    try std.testing.expect(realtimeCoarseS() > 1_577_836_800);
    try std.testing.expect(realtimeCoarseNs() > 1_577_836_800 * std.time.ns_per_s);
}
