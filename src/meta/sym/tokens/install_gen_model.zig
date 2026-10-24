//! The reader's install-under-generation law as an exhaustive model
//! (`docs/porting/sym_tokens_intents_striping.md` §1.7 step 1): the recall
//! handler bumps `revoke_gen` BEFORE it removes the entry; the install
//! checks `gen == gen0` under the cache lock. Weakening: remove-then-bump
//! lets a fetch that read pre-commit pages install after the recall
//! removed the old entry.

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

fn InstallUnderGen(comptime bump_first: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        revoke_gen: u8 = 0,
        /// 0 = none, 1 = the OLD entry (pre-recall), 2 = a FRESH entry
        /// installed from pages read BEFORE the recall landed.
        entry: u8 = 1,
        gen0: u8 = 0,
        /// The fetch's pages predate the recall (its `gen0` read ran before
        /// the bump).
        read_before_bump: bool = false,
        removed: bool = false,
        installed_stale: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 2;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == 0) {
                // The fetch: gen0 read, pages read (pre-commit), then the
                // install under the cache lock checks gen == gen0.
                switch (self.pc[0]) {
                    0 => {
                        self.gen0 = self.revoke_gen;
                        self.read_before_bump = self.revoke_gen == 0;
                    },
                    1 => if (self.revoke_gen == self.gen0) {
                        self.entry = 2;
                        // Stale iff the pages predate the recall and the
                        // recall's removal already ran: the entry would stand
                        // with pre-commit records.
                        self.installed_stale = self.read_before_bump and self.removed;
                    },
                    else => unreachable,
                }
            } else {
                // The recall handler: bump and remove, in the model's order.
                const first = self.pc[1] == 0;
                const do_bump = first == bump_first;
                if (do_bump) self.revoke_gen += 1 else {
                    self.entry = 0;
                    self.removed = true;
                }
            }
            self.pc[t] += 1;
        }
        pub fn invariant(self: *const Self) !void {
            if (self.installed_stale) return error.PreCommitPagesInstalledAfterTheRecall;
        }
        pub fn final(_: *const Self) !void {}
    };
}

test "install under generation: bump-before-remove refuses a fetch that read pre-commit pages; remove-then-bump installs them after the recall" {
    try explorer.expectHolds(InstallUnderGen(true), .{});
    try explorer.expectFails(InstallUnderGen(false), .{}, error.InvariantViolated);
}
