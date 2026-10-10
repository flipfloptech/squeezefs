//! The repair engine (`ops_fsck.md` §8): findings are the ONLY input; the
//! plan is apply's input (dry-run is the DEFAULT and mutates nothing);
//! apply walks the inode plane first (C9 before C10: a destroy deletes a
//! REFERENCER, so every block-class finding naming its blocks verifies as
//! healed), then everything else, stable. Every action VERIFIES BEFORE IT
//! REPAIRS under the object's lease (a healed / stale / moved finding is a
//! `refused` row with its reason, counted, never an error), QUARANTINES the
//! bytes it is about to destroy BEFORE the commit mutation (a crash between
//! the copy and the commit leaves an auditable copy), then commits ONE
//! idempotent step. Report-only classes refuse with their reason. Nothing
//! is ever fabricated: the one-directional C8 release, the DAMAGED flip
//! that leaves the physical block in place for forensics, the raw-key
//! deletion gate that never deletes what it cannot name.

const std = @import("std");
const Allocator = std.mem.Allocator;

const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const tree = @import("../../meta/tree.zig");
const inos = @import("../../meta/inos.zig");
const economy = @import("../../meta/economy.zig");
const layout = @import("../../data/layout.zig");
const block_refs = @import("../../data/block_refs.zig");
const staging = @import("../../data/staging.zig");
const fsck = @import("../fsck.zig");
const finding = @import("finding.zig");
const census = @import("census.zig");
const classes = @import("classes.zig");
const scrub = @import("scrub.zig");

const Finding = finding.Finding;
const Error = census.Error;
const Pass = classes.Pass;
const Host = classes.Host;

pub const Options = struct {
    /// Dry-run (plan only) unless set.
    apply: bool = false,
    quarantine: ?classes.Quarantine.Sink = null,
    /// The kill-9 convergence seam: fires AFTER the quarantine is durable
    /// and BEFORE the commit; true aborts the run (the finding is
    /// re-detected by the next run; every action is idempotent).
    abort_hook: ?*const fn (Finding) bool = null,
    /// The scrub's verifier (C2-lost's content arm, C7's re-verify).
    verifier: ?*scrub.Verifier = null,
};

const Row = finding.RepairRow;

/// The apply order: the inode plane first, C9 before C10, then everything
/// else in identity order (stable).
fn applyRank(f: Finding) u8 {
    return switch (f) {
        .c9_unreferenced => 0,
        .c10_nlink_too_high, .c10_nlink_too_low, .c10_zero_nlink_named, .c10_dangling => 1,
        else => 2,
    };
}

fn rankLess(_: void, a: Finding, b: Finding) bool {
    const ra = applyRank(a);
    const rb = applyRank(b);
    if (ra != rb) return ra < rb;
    return a.order(b) == .lt;
}

pub const Engine = struct {
    pass: *Pass,
    gpa: Allocator,
    opts: Options,
    q: classes.Quarantine,
    rows: std.ArrayList(Row) = .empty,
    /// A raise re-walks the census before the next block-class verify.
    census_stale: bool = false,
    aborted: bool = false,

    pub fn init(pass: *Pass, opts: Options) Engine {
        return .{ .pass = pass, .gpa = pass.gpa, .opts = opts, .q = .{ .sink = opts.quarantine } };
    }

    /// Plan (and, with `apply`, execute) every finding of `findings`.
    pub fn run(self: *Engine, findings: []const Finding) Error!finding.RepairReport {
        const gpa = self.gpa;
        const ordered = try gpa.dupe(Finding, findings);
        defer gpa.free(ordered);
        std.mem.sort(Finding, ordered, {}, rankLess);
        for (ordered) |f| {
            if (self.aborted or self.pass.host.cancelled()) break;
            const verb = f.verb();
            if (verb == .report_only) {
                try self.push(f, .refused, reportOnlyReason(f));
                continue;
            }
            if (!self.opts.apply) {
                fsck.stats.fsck_repairs_planned.inc();
                try self.push(f, .planned, planDetail(f));
                continue;
            }
            const outcome = self.applyOne(f) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => Outcome{ .refused = "the store refused the step (the finding stands for the next run)" },
            };
            switch (outcome) {
                .applied => |d| {
                    fsck.stats.fsck_repairs_applied.inc();
                    fsck.repairClassCounter(@backingInt(f.class())).inc();
                    try self.push(f, .applied, d);
                },
                .refused => |d| try self.push(f, .refused, d),
            }
        }
        return .{ .applied_mode = self.opts.apply, .rows = try self.rows.toOwnedSlice(gpa), .quarantined_records = self.q.records, .quarantined_blocks = self.q.blocks, .quarantined_bytes = self.q.bytes };
    }

    fn push(self: *Engine, f: Finding, o: finding.Outcome, detail: []const u8) Allocator.Error!void {
        if (o == .refused) fsck.stats.fsck_repairs_refused.inc();
        try self.rows.append(self.gpa, .{ .finding = f, .verb = f.verb(), .outcome = o, .detail = detail });
    }

    const Outcome = union(enum) { applied: []const u8, refused: []const u8 };

    fn hook(self: *Engine, f: Finding) bool {
        const h = self.opts.abort_hook orelse return false;
        if (h(f)) {
            self.aborted = true;
            return true;
        }
        return false;
    }

    fn applyOne(self: *Engine, f: Finding) Error!Outcome {
        return switch (f) {
            .c1_torn => .{ .applied = "quarantine-report-only: the cursor and the error are in the manifest; no mutation (the finding persists by design)" },
            .c1_semantic => |x| self.c1Rebuild(f, x.key, "the record now satisfies its schema"),
            .c1_raw_key => |x| self.c1Rebuild(f, x.key, "the key now decodes"),
            .c2_leaked => |x| self.c2Leaked(f, x.vol_tag, x.block_idx),
            .c2_lost => |x| if (x.unrepairable_shape) self.quarantineMapping(f, x.ino, x.block_index, x.vol_tag, x.block_idx) else self.c2LostVerify(f, x.ino, x.block_index, x.vol_tag, x.block_idx),
            .c3_refcount => |x| self.c3Recount(f, x.vol_tag, x.block_idx),
            .c4_orphan => |x| self.c4Discard(f, x.file_id, x.ino),
            .c5_staging => self.c5Quarantine(f),
            .c6_drift => |x| self.c6Recompute(x.vol_tag),
            .c7_scrub => |x| self.quarantineMapping(f, x.ino, x.block_index, x.vol_tag, x.block_idx),
            .c8_drift => |x| self.c8Release(f, x.vol_tag, x.block_idx, x.durable, x.derived),
            .c9_unreferenced => |x| self.c9Destroy(f, x.ino),
            .c10_nlink_too_high, .c10_nlink_too_low, .c10_zero_nlink_named => self.c10Adjust(f),
            .c10_dangling => |x| self.c10RemoveDangling(f, x.parent, x.suffix, x.child),
            .c13_orphan_image => |x| self.c13Return(x.vol, x.appender, x.extent),
            .c15_unrecovered_appender => |x| self.c15Recover(x.vol, x.appender),
            else => unreachable, // report-only arms never reach apply
        };
    }

    // ---- the per-class arms ----

    /// `rebuild-in-place`: quarantine key + value, then `Delete` (an
    /// ordinary journaled CoW re-emit); refused if the record healed.
    fn c1Rebuild(self: *Engine, f: Finding, k: key.KeyBuf, healed: []const u8) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        // The deletion gate: a KNOWN kind in a shape it never takes is
        // dropped under its declared kind; an unknown kind / a truncated
        // key is never deleted.
        const kind: u8 = key.slotTreeKeyKind(k.slice()) catch blk: {
            const d = key.classifyRefusedKey(k.slice()) orelse unreachable;
            break :blk switch (d) {
                .malformed_known_kind => |m| m.kind,
                .slot_out_of_namespace => |s| s.kind,
                .unknown_kind, .truncated => return .{ .refused = "the raw repair never deletes what it cannot name (unknown kind / truncated)" },
            };
        };
        const g = host.lockIno(if (k.slice().len >= 8) std.mem.readInt(u64, k.slice()[0..8], .big) else 0);
        defer host.unlock(g);
        const v = (try host.store.lookup(gpa, kind, k.slice())) orelse return .{ .refused = "vanished: the record is gone" };
        defer gpa.free(v);
        if (f == .c1_semantic and classes.recordSatisfiesSchema(kind, k.slice(), v, host.geo)) return .{ .refused = healed };
        try self.q.putRecord(.c1, "record", k.slice());
        try self.q.putRecord(.c1, "record-value", v);
        if (self.hook(f)) return .{ .refused = "aborted after the quarantine (re-detected next run)" };
        const t = try host.store.begin(@src());
        errdefer host.store.abandon(t);
        try t.stageDelete(kind, k.slice());
        try host.store.commit(t);
        return .{ .applied = "deleted after quarantine" };
    }

    /// `free-leaked-block`: refused if tracked no more, referenced or
    /// in-flight now, OR if the durable ledger still names the block (that
    /// block is C8's to judge — freeing it under a standing record would
    /// seed it LIVE at the next mount while the bitmap reads clear).
    fn c2Leaked(self: *Engine, f: Finding, vol_tag: u64, idx: u64) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        const v = host.volume(vol_tag) orelse return .{ .refused = "unknown volume" };
        if (v.alloc.refcount(idx) == null) return .{ .refused = "healed: untracked now" };
        if (v.alloc.inflightContains(idx)) return .{ .refused = "a live owner holds the block in flight" };
        const durable = block_refs.refcount(host.store.refStore(), gpa, vol_tag, idx) catch return .{ .refused = "the ledger could not be read" };
        if (durable != 0) return .{ .refused = "the durable ledger names the block — C8's to judge" };
        const c = try self.freshCensus();
        for (c.owners.items) |o| for (o.bindings) |b| if (b.vol_tag == vol_tag and b.block_idx == idx) return .{ .refused = "referenced now (a publish landed)" };
        try self.quarantineBlockBytes(.c2, vol_tag, idx);
        if (self.hook(f)) return .{ .refused = "aborted after the quarantine (re-detected next run)" };
        return switch (v.alloc.freeBlock(idx) catch return .{ .refused = "the allocator refused the free" }) {
            .terminal => .{ .applied = "freed (begin → purge → finish)" },
            .nonterminal => .{ .refused = "another owner remains" },
            .refused => .{ .refused = "the allocator refused the free (untracked / shared / shipped)" },
        };
    }

    /// `verify-content-then-repair-allocator-or-quarantine`: the content
    /// verifies (the C7 check) ⇒ recover the block + set the refcount (the
    /// data was fine, the accounting was wrong); else quarantine the mapping.
    fn c2LostVerify(self: *Engine, f: Finding, ino: u64, block_index: u32, vol_tag: u64, idx: u64) Error!Outcome {
        const host = self.pass.host;
        const v = host.volume(vol_tag) orelse return .{ .refused = "unknown volume" };
        if (v.alloc.refcount(idx) != null) return .{ .refused = "healed: tracked now" };
        const m = (try self.currentMapping(ino, block_index, vol_tag, idx)) orelse return .{ .refused = "the mapping is no longer current" };
        const verifier = self.opts.verifier orelse return .{ .refused = "no block reader: the content cannot be verified" };
        const ok = verifier.verifyMapping(m) catch false;
        if (!ok) return self.quarantineMapping(f, ino, block_index, vol_tag, idx);
        const c = try self.freshCensus();
        var n: u32 = 0;
        for (c.owners.items) |o| for (o.bindings) |b| if (b.vol_tag == vol_tag and b.block_idx == idx) {
            n += 1;
        };
        if (n == 0) return .{ .refused = "unreferenced now" };
        if (self.hook(f)) return .{ .refused = "aborted before the allocator step (re-detected next run)" };
        _ = v.alloc.allocateSpecificBlock(idx) catch return .{ .refused = "the allocator could not claim the block" };
        v.alloc.fsckSetRefcount(idx, n);
        v.alloc.publishBlock(idx);
        return .{ .applied = "content verified; the allocator now tracks the block at the counted references" };
    }

    /// `recount-and-set-refcount`: every referencing ino's lease ASCENDING,
    /// the references recounted UNDER them, `fsckSetRefcount(counted)`.
    fn c3Recount(self: *Engine, f: Finding, vol_tag: u64, idx: u64) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        const v = host.volume(vol_tag) orelse return .{ .refused = "unknown volume" };
        const rc = v.alloc.refcount(idx) orelse return .{ .refused = "untracked: C2's object" };
        const c = try self.freshCensus();
        var owners: std.ArrayList(u64) = .empty;
        defer owners.deinit(gpa);
        for (c.owners.items) |o| for (o.bindings) |b| if (b.vol_tag == vol_tag and b.block_idx == idx) try owners.append(gpa, o.ino);
        if (owners.items.len == 0) return .{ .refused = "unreferenced: C2's object" };
        std.mem.sort(u64, owners.items, {}, std.sort.asc(u64));
        var guards = host.lockInos(owners.items) catch return .{ .refused = "more referencers than one lease plan holds (re-run after the others repair)" };
        defer host.unlockInos(&guards);
        var counted: u32 = 0;
        for (owners.items) |o| {
            var binds: std.ArrayList(census.MappingRef) = .empty;
            defer binds.deinit(gpa);
            _ = try host.currentBindings(gpa, o, &binds);
            for (binds.items) |b| if (b.mapping.vol_tag == vol_tag and b.mapping.blockIdx(host.geo.chunk) == idx) {
                counted += 1;
            };
        }
        if (counted == 0) return .{ .refused = "unreferenced under the leases" };
        if (counted == rc) return .{ .refused = "healed: the refcount matches" };
        if (self.hook(f)) return .{ .refused = "aborted before the set (re-detected next run)" };
        v.alloc.fsckSetRefcount(idx, counted);
        return .{ .applied = "refcount set to the recounted references" };
    }

    /// `quarantine-then-discard-custody`: the verbatim record image into
    /// quarantine, then the retire at its generation.
    fn c4Discard(self: *Engine, f: Finding, file_id: [16]u8, ino: u64) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        const face = host.staging orelse return .{ .refused = "no staging dir" };
        const g = host.lockIno(ino);
        defer host.unlock(g);
        const sk = staging.StagingKey.init(file_id, ino);
        const img = (try face.vtable.image(face.ctx, gpa, sk)) orelse return .{ .refused = "healed: the custody is gone" };
        defer gpa.free(img);
        if (try host.readInode(gpa, ino)) |v| if (v.nlink != 0) return .{ .refused = "healed: the ino is live" };
        try self.q.putRecord(.c4, "staged-custody", img);
        if (self.hook(f)) return .{ .refused = "aborted after the quarantine (re-detected next run)" };
        const residents = try face.vtable.residents(face.ctx, gpa);
        defer gpa.free(residents);
        for (residents) |r| if (std.mem.eql(u8, std.mem.asBytes(&r.key), std.mem.asBytes(&sk))) {
            return if (face.vtable.remove(face.ctx, sk, r.generation)) .{ .applied = "custody retired after quarantine" } else .{ .refused = "the record moved under us" };
        };
        return .{ .refused = "healed: the custody is gone" };
    }

    fn c5Quarantine(self: *Engine, f: Finding) Error!Outcome {
        const face = self.pass.host.staging orelse return .{ .refused = "no staging dir" };
        if (face.vtable.marker(face.ctx) == null) return .{ .refused = "healed: the marker rebinds" };
        if (self.hook(f)) return .{ .refused = "aborted before the move-aside (re-detected next run)" };
        try face.vtable.quarantineDir(face.ctx, &self.q);
        return .{ .applied = "the staging dir copied then moved aside" };
    }

    fn c6Recompute(self: *Engine, vol_tag: u64) Error!Outcome {
        const v = self.pass.host.volume(vol_tag) orelse return .{ .refused = "unknown volume" };
        const r = v.alloc.fsckReconcileAccounting();
        return .{ .applied = if (r.frees_completed + r.free_list_evictions == 0) "recomputed: nothing to reconcile" else "recomputed: frees completed and free-list evictions applied" };
    }

    /// `release-stale-block-refs` (OFFLINE only): every record of the block
    /// judged BEFORE any is released; an unreadable justification refuses
    /// the whole finding; the other direction is never fabricated.
    fn c8Release(self: *Engine, f: Finding, vol_tag: u64, idx: u64, durable: u32, derived: u32) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        if (host.online()) return .{ .refused = "online: the verification read and the release must not interleave with a live publish" };
        const rows = [_]block_refs.Drift{.{ .block_idx = idx, .durable = durable, .derived = derived }};
        const J = struct {
            host: *const Host,
            gpa: Allocator,
            fn names(ctx: ?*anyopaque, owner_ino: u64, block_index: u32, block_idx: u64) block_refs.Naming {
                const self_: *@This() = @ptrCast(@alignCast(ctx.?));
                var binds: std.ArrayList(census.MappingRef) = .empty;
                defer binds.deinit(self_.gpa);
                const present = self_.host.currentBindings(self_.gpa, owner_ino, &binds) catch return .unknown;
                if (!present) return .not_named;
                for (binds.items) |b| if (b.block_index == block_index and b.mapping.blockIdx(self_.host.geo.chunk) == block_idx) return .named;
                return .not_named;
            }
        };
        var j: J = .{ .host = host, .gpa = gpa };
        var plan = block_refs.repairPlan(gpa, host.store.refStore(), vol_tag, .{ .rows = @constCast(&rows), .pack_exempted = 0, .inflight_exempted = 0 }, .{ .ctx = &j, .namesBlock = &J.names }, host.online()) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.OnlineRepairRefused => .{ .refused = "online repair refused" },
            error.UnknownJustification => .{ .refused = "a record's justification could not be read: nothing released" },
            else => .{ .refused = "the ledger could not be read" },
        };
        defer plan.deinit(gpa);
        if (plan.releases.len == 0) return .{ .refused = if (plan.unfabricated != 0) "the layout names a block with no record: never fabricated (report-only direction)" else "healed: no stale record" };
        for (plan.releases) |r| try self.q.putRecord(.c8, "block-ref", &r.encodeKey());
        if (self.hook(f)) return .{ .refused = "aborted after the quarantine (re-detected next run)" };
        const t = try host.store.begin(@src());
        errdefer host.store.abandon(t);
        try block_refs.stageRepair(t, plan);
        try host.store.commit(t);
        return .{ .applied = "stale durable references released (one-directional)" };
    }

    /// `destroy-unreferenced-inode`: intent ⇒ refuse; verify under the
    /// lease (live, prior-era, the repair-time dentry pass does NOT name
    /// it; an incomplete pass REFUSES); quarantine the inode + EVERY xattr
    /// record; then ONE tx: the durable references released + the destroy;
    /// the blocks through the terminal-free law after.
    fn c9Destroy(self: *Engine, f: Finding, global: u64) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        if (host.intents) |i| if (i.isOpen(i.ctx, global)) return .{ .refused = "an open cross-volume intent names the inode" };
        const sl = census.ofGlobalIno(global) orelse return .{ .refused = "not a routable ino" };
        const kino = census.keyInoOf(sl);
        const g = host.lockIno(kino);
        defer host.unlock(g);
        const v = (try host.readInode(gpa, kino)) orelse return .{ .refused = "vanished (the crashed-create shape already reclaimed)" };
        if (v.nlink == 0) return .{ .refused = "nlink 0: the mount's corpse sweep owns it" };
        if (host.scope.era(sl) != .prior) return .{ .refused = "current era: never destroyed under a live creator" };
        const d = (try self.freshDentries()) orelse return .{ .refused = "the repair-time dentry pass is incomplete: rather than destroy an inode whose name may exist" };
        if (d.namesOf(global) != 0) return .{ .refused = "named now (a late InsertDentry / a reconnect)" };
        // Quarantine the inode record and every xattr record (the layout
        // is the only map to the blocks; the blocks' KEYS go to the manifest).
        const ik = key.inodeKey(kino);
        try self.q.putRecord(.c9, "inode", &ik);
        try self.q.putRecord(.c9, "inode-value", &v.encode());
        var xkeys: std.ArrayList([]u8) = .empty;
        defer {
            for (xkeys.items) |k| gpa.free(k);
            xkeys.deinit(gpa);
        }
        var binds: std.ArrayList(census.MappingRef) = .empty;
        defer binds.deinit(gpa);
        _ = try host.currentBindings(gpa, kino, &binds);
        {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(gpa, &kvs);
            try host.store.range(gpa, key.TREE_XATTRS, &key.xattrKey(kino, 0, 0), &key.xattrKey(kino, key.HASH56_MAX, std.math.maxInt(u8)), std.math.maxInt(usize), &kvs);
            for (kvs.items) |kv| {
                if (kv.key.len != key.XATTR_KEY_LEN or kv.key[key.INO_MAJOR_KIND_OFF] != key.TREE_XATTRS) continue;
                try self.q.putRecord(.c9, "xattr", kv.key);
                try self.q.putRecord(.c9, "xattr-value", kv.value);
                try xkeys.append(gpa, try gpa.dupe(u8, kv.key));
            }
        }
        for (binds.items) |b| {
            var label: [64]u8 = undefined;
            const s = std.fmt.bufPrint(&label, "block vol-{x:0>16} {d}", .{ b.mapping.vol_tag, b.mapping.blockIdx(host.geo.chunk) }) catch "block";
            try self.q.putRecord(.c9, s, &.{});
        }
        if (self.hook(f)) return .{ .refused = "aborted after the quarantine (re-detected next run)" };
        const t = try host.store.begin(@src());
        errdefer host.store.abandon(t);
        for (binds.items) |b| try block_refs.stageOps(t, &.{block_refs.released(.{ .vol_tag = b.mapping.vol_tag, .block_idx = b.mapping.blockIdx(host.geo.chunk), .owner_ino = kino, .block_index = b.block_index })});
        try inos.stageDestroy(t, kino, xkeys.items);
        try host.store.commit(t);
        for (binds.items) |b| if (host.volume(b.mapping.vol_tag)) |vol| {
            const idx = b.mapping.blockIdx(host.geo.chunk);
            if (idx < vol.alloc.capacityBlocks()) _ = vol.alloc.freeBlock(idx) catch {};
        };
        self.census_stale = true;
        return .{ .applied = "inode destroyed after quarantine; references released; blocks through the terminal-free law" };
    }

    /// ONE ladder for the three count arms: intent ⇒ refuse; incomplete
    /// pass ⇒ refuse; directory ⇒ refuse; `names == 0` ⇒ refuse (C9's);
    /// equal ⇒ healed; direction reversed ⇒ refuse; quarantine; adjust.
    fn c10Adjust(self: *Engine, f: Finding) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        const global: u64 = switch (f) {
            .c10_nlink_too_high, .c10_nlink_too_low => |x| x.ino,
            .c10_zero_nlink_named => |x| x.ino,
            else => unreachable,
        };
        if (host.intents) |i| if (i.isOpen(i.ctx, global)) return .{ .refused = "an open cross-volume intent names the inode" };
        const d = (try self.freshDentries()) orelse return .{ .refused = "the repair-time dentry pass is incomplete" };
        const sl = census.ofGlobalIno(global) orelse return .{ .refused = "not a routable ino" };
        const kino = census.keyInoOf(sl);
        const g = host.lockIno(kino);
        defer host.unlock(g);
        var v = (try host.readInode(gpa, kino)) orelse return .{ .refused = "vanished" };
        if (economy.isDirMode(v.mode)) return .{ .refused = "a directory is never a count finding" };
        const names = d.namesOf(global);
        if (names == 0) return .{ .refused = "no name: C9's object" };
        if (v.nlink == names) return .{ .refused = "healed: nlink equals the names" };
        const lowering = v.nlink > names;
        if (lowering != (f == .c10_nlink_too_high)) return .{ .refused = "direction reversed: re-run detection rather than apply a stale verdict" };
        if (lowering and self.pass.host.online()) {
            // The one C10 repair that could make a named inode reclaimable
            // needs the record unchanged across a third independent pass.
            const again = (try host.readInode(gpa, kino)) orelse return .{ .refused = "vanished" };
            if (again.nlink != v.nlink or again.ctime != v.ctime) return .{ .refused = "the record moved under the lease" };
        }
        try self.q.putRecord(.c10, "inode", &key.inodeKey(kino));
        try self.q.putRecord(.c10, "inode-value", &v.encode());
        if (self.hook(f)) return .{ .refused = "aborted after the quarantine (re-detected next run)" };
        v.nlink = names;
        const t = try host.store.begin(@src());
        errdefer host.store.abandon(t);
        try t.stagePut(key.TREE_INODES, &key.inodeKey(kino), &v.encode());
        try host.store.commit(t);
        if (!lowering) self.census_stale = true;
        return .{ .applied = if (lowering) "nlink lowered to the counted names" else "nlink raised to the counted names" };
    }

    /// `remove-dangling-dentry` under the PARENT's lease: the record
    /// present verbatim, decodes, still names this child, `DT_DIR`
    /// refuses, the child still absent; quarantine key + value; delete.
    fn c10RemoveDangling(self: *Engine, f: Finding, parent: u64, suffix: u64, child: u64) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        const g = host.lockIno(parent);
        defer host.unlock(g);
        const dk = key.dentryKey(parent, suffix >> 8, @truncate(suffix));
        const v = (try host.store.lookup(gpa, key.TREE_DENTRIES, &dk)) orelse return .{ .refused = "healed: the dentry is gone" };
        defer gpa.free(v);
        const dv = record.DentryValue.decode(v) catch return .{ .refused = "the record no longer decodes: C1's object" };
        if (dv.child_ino != child) return .{ .refused = "the name points elsewhere now" };
        if (dv.file_type == economy.ftByte(economy.S.IFDIR)) return .{ .refused = "DT_DIR: the removal must also decide the parent's directory nlink" };
        if (census.ofGlobalIno(child)) |csl| if ((try host.readInode(gpa, census.keyInoOf(csl))) != null) return .{ .refused = "the child exists now (the create window)" };
        try self.q.putRecord(.c10, "dentry", &dk);
        try self.q.putRecord(.c10, "dentry-value", v);
        if (self.hook(f)) return .{ .refused = "aborted after the quarantine (re-detected next run)" };
        const t = try host.store.begin(@src());
        errdefer host.store.abandon(t);
        try t.stageDelete(key.TREE_DENTRIES, &dk);
        try host.store.commit(t);
        return .{ .applied = "dangling dentry removed after quarantine" };
    }

    fn c13Return(self: *Engine, vol: u16, appender: u32, extent: u64) Error!Outcome {
        const face = self.pass.host.forest orelse return .{ .refused = "no forest" };
        return if (try face.vtable.returnOrphan(face.ctx, vol, appender, extent)) .{ .applied = "orphan image extent returned through the appender's ring" } else .{ .refused = "healed: the extent is reached or returned" };
    }

    fn c15Recover(self: *Engine, vol: u16, appender: u32) Error!Outcome {
        const host = self.pass.host;
        if (!host.online()) return .{ .refused = "offline: the mount path recovers a dead appender before serving (a probe holds no ring and no lease to recover under)" };
        const face = host.forest orelse return .{ .refused = "no forest" };
        return if (try face.vtable.recoverDead(face.ctx, vol, appender)) .{ .applied = "dead appender recovered" } else .{ .refused = "healed: the ledger poll recovered it" };
    }

    // ---- shared steps ----

    /// `quarantine-mapping`: re-read under the lease, quarantine what is
    /// readable, flip to DAMAGED (reads EIO); the physical block stays in
    /// place for forensics. Supersession-safe: the flip lands only where
    /// the captured mapping is STILL current.
    fn quarantineMapping(self: *Engine, f: Finding, ino: u64, block_index: u32, vol_tag: u64, idx: u64) Error!Outcome {
        const host = self.pass.host;
        const gpa = self.gpa;
        const g = host.lockIno(ino);
        defer host.unlock(g);
        const rec = (try host.readLayout(gpa, ino)) orelse return .{ .refused = "the owner has no layout now" };
        defer gpa.free(rec.value);
        if (rec.layout.isKvmap()) return .{ .refused = "a kvmap-mapped block: the map plane is report-only here" };
        var flipped = rec.layout;
        var entries: std.ArrayList(layout.MapEntryWire) = .empty;
        defer entries.deinit(gpa);
        var found = false;
        if (rec.layout.durableCopy()) |m| {
            if (block_index == 0 and m.vol_tag == vol_tag and m.blockIdx(host.geo.chunk) == idx and !m.damaged) {
                flipped.durable_copy.damaged = true;
                found = true;
            }
        }
        if (rec.layout.flags & layout.F_MAP_INLINE != 0) {
            try entries.appendSlice(gpa, rec.layout.entries);
            for (entries.items, 0..) |e, i| {
                const m = rec.layout.entry(host.geo.chunk, i).mapping;
                if (e.block() == block_index and m.vol_tag == vol_tag and m.blockIdx(host.geo.chunk) == idx and !m.damaged) {
                    var dm = m;
                    dm.damaged = true;
                    entries.items[i].mapping = dm.encode();
                    found = true;
                }
            }
            flipped.entries = entries.items;
        }
        if (!found) return .{ .refused = "the mapping is no longer current (superseded) or already quarantined" };
        try self.quarantineBlockBytes(f.class(), vol_tag, idx);
        if (self.hook(f)) return .{ .refused = "aborted after the quarantine (re-detected next run)" };
        const t = try host.store.begin(@src());
        errdefer host.store.abandon(t);
        layout.publishLayout(t, .{ .ino = ino, .hash_seed = host.hash_seed, .coll_seq = rec.coll_seq, .prev = rec.layout, .layout = flipped, .proofs = &.{} }, classes.refs_seam, host.geo) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => .{ .refused = "the flipped layout was refused by the publish law" },
        };
        try host.store.commit(t);
        return .{ .applied = "mapping flipped to DAMAGED after quarantine (reads EIO; the block stays for forensics)" };
    }

    /// The block's bytes into quarantine, best-effort (no reader ⇒ the
    /// manifest names the block).
    fn quarantineBlockBytes(self: *Engine, class: finding.Class, vol_tag: u64, idx: u64) Error!void {
        const host = self.pass.host;
        var label: [64]u8 = undefined;
        const s = std.fmt.bufPrint(&label, "block vol-{x:0>16} {d}", .{ vol_tag, idx }) catch "block";
        const r = host.reader orelse return self.q.putBlock(class, s, &.{});
        const chunk: usize = @intCast(host.geo.chunk);
        const buf = try self.gpa.alloc(u8, chunk);
        defer self.gpa.free(buf);
        r.read(r.ctx, vol_tag, idx * host.geo.chunk, buf) catch return self.q.putBlock(class, s, &.{});
        try self.q.putBlock(class, s, buf);
    }

    fn currentMapping(self: *Engine, ino: u64, block_index: u32, vol_tag: u64, idx: u64) Error!?layout.Mapping {
        const host = self.pass.host;
        var binds: std.ArrayList(census.MappingRef) = .empty;
        defer binds.deinit(self.gpa);
        if (!(try host.currentBindings(self.gpa, ino, &binds))) return null;
        for (binds.items) |b| if (b.block_index == block_index and b.mapping.vol_tag == vol_tag and b.mapping.blockIdx(host.geo.chunk) == idx) return b.mapping;
        return null;
    }

    /// ONE fresh census for the block classes (re-walked after an inode-
    /// plane raise).
    fn freshCensus(self: *Engine) Error!*census.Census {
        if (self.census_stale) {
            if (self.pass.fresh.cen) |*c| c.deinit();
            self.pass.fresh.cen = null;
            self.census_stale = false;
        }
        if (self.pass.fresh.cen == null) {
            var tags_buf: [64]u64 = undefined;
            const tags = self.pass.host.knownTags(&tags_buf);
            self.pass.fresh.cen = try census.run(self.gpa, self.pass.host.store, .{ .scope = self.pass.host.scope, .shard = self.pass.host.shard, .mem_budget = self.pass.host.mem_budget, .geo = self.pass.host.geo, .known_vol_tags = tags });
        }
        return &self.pass.fresh.cen.?;
    }

    /// ONE repair-time dentry pass carrying C9's "is it named" and C10's
    /// deduped identities; null = incomplete (every inode-plane action refuses).
    fn freshDentries(self: *Engine) Error!?*census.DentryPass {
        if (self.pass.fresh.dentries == null) {
            var collect: std.AutoHashMapUnmanaged(u64, void) = .empty;
            defer collect.deinit(self.gpa);
            for (self.pass.findings.items) |s| switch (s) {
                .c9_unreferenced => |x| try collect.put(self.gpa, x.ino, {}),
                .c10_nlink_too_high, .c10_nlink_too_low => |x| try collect.put(self.gpa, x.ino, {}),
                .c10_zero_nlink_named => |x| try collect.put(self.gpa, x.ino, {}),
                else => {},
            };
            const c = try self.freshCensus();
            self.pass.fresh.dentries = try census.dentryPass(self.gpa, self.pass.host.store, .{ .scope = self.pass.host.scope, .shard = self.pass.host.shard, .mem_budget = self.pass.host.mem_budget, .ceiling = c.rawCeiling(), .collect = &collect, .stripes = false });
        }
        const d = &self.pass.fresh.dentries.?;
        return if (d.complete) d else null;
    }
};

fn reportOnlyReason(f: Finding) []const u8 {
    return switch (f) {
        .c1_raw_key => "the raw repair never deletes what it cannot name (an unknown kind may be a later binary's record; a truncated key names no kind)",
        .c11_orphan_map_records, .c11_empty_kvmap_head, .c11_run_foreign_shadow => "the map plane is report-only: a false quarantine would hole a LIVE crossing's staged map; the residue sweep is the reclaim path",
        .c12_overlap => "at least one tenant is wrong and nothing on the volume says which — quarantining both would destroy the right one",
        .c12_overrun => "an unreadable window's base IS referenced; restating the window would fabricate a mapping",
        .c14_slot_custody_conflict => "the mount refuses on it; the remedy is the operator's attestation `squeezefs appender clear <uri> <id>`",
        .c16_shared_index_drift => "the flag and the index are two durable homes of one fact; restating one from the other would erase the evidence of which side lied",
        .c17_stripe => "stripe maps are report-only (the C8 fabricate-direction posture)",
        else => "report-only",
    };
}

fn planDetail(f: Finding) []const u8 {
    return switch (f.verb()) {
        .quarantine_report_only => "record the cursor and the subtree's key range in the manifest; no mutation",
        .rebuild_in_place => "quarantine key + value, then delete the record (a journaled CoW leaf re-emit)",
        .free_leaked_block => "quarantine the block bytes, then free it (refused if the durable ledger names it)",
        .quarantine_mapping => "quarantine the readable bytes, flip the mapping to DAMAGED (reads EIO; the block stays)",
        .verify_content_then_repair_allocator_or_quarantine => "verify the content; good ⇒ recover the block at the counted references, else quarantine the mapping",
        .recount_and_set_refcount => "lease every referencer ascending, recount, set the refcount",
        .quarantine_then_discard_custody => "copy the custody image, then retire it at its generation",
        .quarantine_staging_dir => "copy the marker and every segment, then move the dir aside",
        .recompute_accounting => "the allocator's reconcile: complete limbo frees, evict tracked blocks from the free list",
        .release_stale_block_refs => "judge every record of the block, release those whose owner's layout does not name it (offline only)",
        .destroy_unreferenced_inode => "quarantine the inode + every xattr, release its references, destroy in ONE tx",
        .lower_nlink_to_counted_names => "quarantine the record, set nlink to the deduped names (a third pass must agree)",
        .raise_nlink_to_counted_names => "quarantine the record, raise nlink to the deduped names (the census re-walks after)",
        .remove_dangling_dentry => "under the parent's lease: quarantine the dentry, delete it",
        .return_orphan_image_extent => "one free(extent) record in the appender's ring; nothing quarantined (no route reaches the image)",
        .recover_dead_appender => "the §5.9 recovery on the volume's manager (online only)",
        .report_only => "report-only",
    };
}
