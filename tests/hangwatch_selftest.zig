//! The hang watchdog's self-test binary (`tests/test_runner.zig`, THE HANG
//! WATCHDOG). build.zig runs it in terminal mode with `--hang-limit-ms=2000`
//! and expects: the two 1.2 s tests pass — together they outlast the limit,
//! so the limit is per test — and the spinning third ends the process with
//! the HUNG line, the thread dump and exit 124.
const std = @import("std");

test "hangwatch: a quick test passes (1.2 s)" {
    try std.testing.io.sleep(.fromMilliseconds(1200), .awake);
}

test "hangwatch: a second quick test passes (1.2 s)" {
    try std.testing.io.sleep(.fromMilliseconds(1200), .awake);
}

test "hangwatch: a spinning test never returns" {
    var spins: u64 = 0;
    while (true) {
        spins +%= 1;
        std.mem.doNotOptimizeAway(spins);
    }
}
