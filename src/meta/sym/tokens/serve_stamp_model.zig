//! The reader's two generation laws as exhaustive models (`docs/porting/
//! sym_tokens_intents_striping.md` §1.5 last paragraph, §1.7 step 1):
//!
//! 1. The `ServeStamp` drain — a Dekker under TSO. A SERVE loads the
//!    generation, stamps the slot of the generation it read (a store that
//!    may sit in its store buffer), FENCES (the seq_cst re-read of the
//!    generation), then resolves its binding; a serve that sees the
//!    generation moved resolves AFTER the step (fresh). The DRAIN publishes
//!    the step's pre-step marks, stores the new generation, fences, then
//!    loads the old slot: 0 ⇒ drained. Invariant: a serve holding a
//!    PRE-STEP binding is never in flight after the drain completed.
//!    Weakening: the serve's fence dropped (the re-read relaxed) — both
//!    sides read the other's old value: the serve believes no step ran and
//!    serves the pre-step binding, the drain believes the slot empty.
//!
//! 2. Install-under-generation — the recall handler bumps `revoke_gen`
//!    BEFORE it removes the entry; the install checks `gen == gen0` under
//!    the cache lock. Weakening: remove-then-bump lets a fetch that read
//!    pre-commit pages install after the recall removed the old entry.

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

fn ServeDrain(comptime serve_fence: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        /// The published generation (0 = old, 1 = stepped) and the binding
        /// version the step publishes BEFORE the generation.
        gen: u8 = 0,
        version: u8 = 0,
        /// The old generation's slot.
        slot0: u8 = 0,
        /// Per-thread store buffers.
        pending_slot: bool = false,
        pending_gen: bool = false,
        /// The serve's read of the generation at its start and the binding
        /// version it served (null = not yet).
        g: u8 = 0,
        served_version: ?u8 = null,
        drained: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == @as(u8, if (t == 0) 5 else 4);
        }
        fn drainSlot(self: *Self) void {
            if (self.pending_slot) {
                self.slot0 = 1;
                self.pending_slot = false;
            }
        }
        fn drainGen(self: *Self) void {
            if (self.pending_gen) {
                self.gen = 1;
                self.pending_gen = false;
            }
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == 0) switch (self.pc[0]) {
                0 => self.g = self.gen,
                1 => {
                    // A serve that read the stepped generation stamps the new
                    // slot (not modelled: the drain never waits on it).
                    if (self.g == 0) self.pending_slot = true;
                },
                2 => if (serve_fence) self.drainSlot(),
                3 => {
                    // The Dekker re-read: the generation moved ⇒ resolve after
                    // the step; else resolve against what is visible now.
                    self.served_version = if (self.gen != self.g) 1 else self.version;
                },
                4 => {
                    // The serve ends: the stamp (landed or still buffered) is
                    // gone.
                    self.pending_slot = false;
                    self.slot0 = 0;
                },
                else => unreachable,
            } else switch (self.pc[1]) {
                0 => self.version = 1,
                1 => self.pending_gen = true,
                2 => self.drainGen(),
                3 => {
                    // The drain's slot load: a non-zero slot keeps it waiting
                    // (the pc does not advance — the serve's end unparks it).
                    if (self.slot0 != 0) return;
                    self.drained = true;
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }
        pub fn invariant(self: *const Self) !void {
            // A serve that resolved a PRE-STEP binding and has not ended may
            // not coexist with a completed drain.
            if (self.drained and self.served_version == 0 and self.pc[0] < 5) return error.PreStepServeEscapedTheDrain;
        }
        pub fn final(_: *const Self) !void {}
    };
}

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

test "the ServeStamp drain: with the Dekker re-read no pre-step serve escapes the drain; with it relaxed the serve and the drain both read old values" {
    try explorer.expectHolds(ServeDrain(true), .{});
    try explorer.expectFails(ServeDrain(false), .{}, error.InvariantViolated);
}

test "install under generation: bump-before-remove refuses a fetch that read pre-commit pages; remove-then-bump installs them after the recall" {
    try explorer.expectHolds(InstallUnderGen(true), .{});
    try explorer.expectFails(InstallUnderGen(false), .{}, error.InvariantViolated);
}
