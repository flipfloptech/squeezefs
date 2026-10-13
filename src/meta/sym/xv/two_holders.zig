//! The IN-PROCESS two-holder contracts of the cross-owner plane
//! (`docs/porting/sym_tokens_intents_striping.md` §6 "Cross-owner
//! intents"): two members A and B over the loopback hub with a real wire
//! session each way, each holding its own slots behind a store whose
//! commit is the DOOR (a foreign slot refuses `SlotBusy`), its own 4a
//! table, its own plane. Every op below runs the product's shape — the
//! plan, the travelling guards, the intent, the shipped steps, the
//! retirement — and the counters close (`minted ≡ retired + open`,
//! `shipped ≡ served`). Test-only.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const testing = std.testing;
const errors = @import("../../../core/errors.zig");
const clock = @import("../../../core/clock.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const loopback = @import("../../../wire/loopback.zig");
const auth = @import("../../../wire/auth.zig");
const key = @import("../../key.zig");
const record = @import("../../record.zig");
const tx = @import("../../tx.zig");
const tree = @import("../../tree.zig");
const dlm = @import("../../dlm.zig");
const inos = @import("../../inos.zig");
const economy = @import("../../economy.zig");
const route_table = @import("../../routing/route_table.zig");
const kv_fs = @import("../../../fuse/handler/kv_fs.zig");
const cr = @import("../control_records.zig");
const intent = @import("intent.zig");
const steps = @import("steps.zig");
const applier = @import("applier.zig");
const guards = @import("guards.zig");
const ops = @import("ops.zig");
const dir_rename = @import("dir_rename.zig");
const record_ship = @import("record_ship.zig");
const stripe = @import("stripe.zig");
const token_cache = @import("../tokens/token_cache.zig");

const S = economy.S;
const InodeValue = record.InodeValue;
const W = inos.ROUTING_WIDTH;

pub fn gino(local: u64, slot: u16) u64 {
    return inos.makeGlobalIno(local, slot, W);
}

/// A holder's store: the portable `MemStore` behind the DOOR — a staged
/// record whose slot this holder does not lease refuses `SlotBusy`.
pub const HolderStore = struct {
    pub const StoreError = error{ OutOfMemory, Io, SlotBusy };

    mem: kv_fs.MemStore,
    leased: std.ArrayListUnmanaged(u16) = .empty,
    refusals: u32 = 0,
    commits: u32 = 0,
    /// The commits the last barrier covered (what a crash keeps).
    durable: u32 = 0,

    pub fn coreError(e: StoreError) errors.Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.InputOutput,
            error.SlotBusy => error.SlotMoved,
        };
    }

    pub fn leases(self: *const HolderStore, slot: u16) bool {
        for (self.leased.items) |s| if (s == slot) return true;
        return false;
    }

    pub fn begin(self: *HolderStore, site: tx.Site) Allocator.Error!*tx.KvTx {
        return self.mem.begin(site);
    }

    pub fn abandon(self: *HolderStore, t: *tx.KvTx) void {
        self.mem.abandon(t);
    }

    pub fn commit(self: *HolderStore, t: *tx.KvTx) StoreError!void {
        for (t.staged.items) |s| {
            const fs = key.slotTreeKeySlot(s.key) catch return error.Io;
            const slot = route_table.slotOfForestSlot(fs);
            if (!self.leases(slot)) {
                self.refusals += 1;
                self.mem.abandon(t);
                return error.SlotBusy;
            }
        }
        try self.mem.commit(t);
        self.commits += 1;
    }

    pub fn lookup(self: *HolderStore, gpa: Allocator, kind: u8, k: []const u8) StoreError!?[]u8 {
        return self.mem.lookup(gpa, kind, k);
    }

    pub fn range(self: *HolderStore, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) StoreError!void {
        return self.mem.range(gpa, kind, start, end, max, out);
    }

    pub fn fsync(self: *HolderStore) StoreError!void {
        try self.mem.fsync();
        self.durable = self.commits;
    }

    pub fn nameKey(self: *const HolderStore) *const key.NameKey {
        return self.mem.nameKey();
    }
};

pub const Plane = ops.Plane(HolderStore);
pub const StepService = ops.StepService(HolderStore);

fn linkCall(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
    const c: *loopback.Conn = @ptrCast(@alignCast(ctx));
    return c.call(req);
}

pub fn linkOf(c: *loopback.Conn) token_cache.Link {
    return .{ .ctx = c, .call = &linkCall };
}

/// Tree 0 of volume 0 in RAM (the `dir_rename` record's home at A).
const RamControl = struct {
    rec: ?cr.DirRename = null,
    fn lookup(ctx: *anyopaque, _: []const u8) error{Io}!?cr.DirRename {
        return @as(*RamControl, @ptrCast(@alignCast(ctx))).rec;
    }
    fn put(ctx: *anyopaque, _: []const u8, v: []const u8) error{ Io, Deferred }!void {
        @as(*RamControl, @ptrCast(@alignCast(ctx))).rec = cr.DirRenameWire.decode(v) catch return error.Io;
    }
    fn delete(ctx: *anyopaque, _: []const u8) error{ Io, Deferred }!void {
        @as(*RamControl, @ptrCast(@alignCast(ctx))).rec = null;
    }
    fn store(self: *RamControl) dir_rename.ControlStore {
        return .{ .ctx = self, .lookup = &lookup, .put = &put, .delete = &delete };
    }
};

/// One member.
pub const Holder = struct {
    set: *TwoHolders,
    id: u32,
    store: HolderStore,
    locks: dlm.Manager,
    plane: Plane,
    svc: service.Service = .init,
    step_svc: StepService,
    guards_svc: guards.GuardsService,
    /// This holder's next local ino per slot it mints in (the rotor).
    next_local: u64 = 100,
    /// The session to the OTHER holder.
    conn: loopback.Conn = undefined,
    daemon: usize = 0,
    /// A STALE view: this holder believes the other leases `slot`
    /// whatever its table says (the redirect contracts).
    assume_other_holds: ?u16 = null,
    /// A DEAD view: the first resolution of `slot` names a holder with no
    /// reachable listener (the dead lessee the manager has since replaced).
    unreachable_once: ?u16 = null,

    fn other(self: *Holder) *Holder {
        return if (self.id == 1) &self.set.b else &self.set.a;
    }

    fn homeOf(ctx: *anyopaque, slot: u16) ops.Home {
        const self: *Holder = @ptrCast(@alignCast(ctx));
        if (self.unreachable_once == slot) {
            self.unreachable_once = null;
            return .{ .unreachable_ = .{ .holder = 7 } };
        }
        if (self.store.leases(slot)) return .{ .local = .{ .volume = 0 } };
        const o = self.other();
        if (o.store.leases(slot) or self.assume_other_holds == slot) return .{ .foreign = .{ .holder = o.id, .link = linkOf(&self.conn) } };
        return .{ .unreachable_ = .{ .holder = 0 } };
    }

    fn refresh(ctx: *anyopaque, slot: u16) ops.Home {
        const self: *Holder = @ptrCast(@alignCast(ctx));
        self.set.refreshes += 1;
        return homeOf(ctx, slot);
    }

    fn localSlots(ctx: *anyopaque, out: []u16) []u16 {
        const self: *Holder = @ptrCast(@alignCast(ctx));
        const n = @min(out.len, self.store.leased.items.len);
        @memcpy(out[0..n], self.store.leased.items[0..n]);
        return out[0..n];
    }

    fn linkOfFn(ctx: *anyopaque, holder: u32) ?ops.Link {
        const self: *Holder = @ptrCast(@alignCast(ctx));
        return if (holder == self.other().id) linkOf(&self.conn) else null;
    }

    fn resolver(self: *Holder) ops.Resolver {
        return .{ .ctx = self, .homeOf = &homeOf, .refresh = &refresh, .localSlots = &localSlots, .linkOf = &linkOfFn };
    }

    /// The divert stand-in: a foreign object's record read at its holder.
    fn witnessRead(ctx: *anyopaque, global: u64) errors.Error!?InodeValue {
        const self: *Holder = @ptrCast(@alignCast(ctx));
        return self.other().plane.apply.readInode(global) catch error.InputOutput;
    }

    fn witness(self: *Holder) ops.Witness {
        return .{ .ctx = self, .readInode = &witnessRead };
    }

    /// Mint a child in this holder's own rotor (its first slot).
    pub fn mint(self: *Holder) u64 {
        const l = self.next_local;
        self.next_local += 1;
        return gino(l, self.store.leased.items[0]);
    }

    pub fn inode(self: *Holder, global: u64) !?InodeValue {
        return self.plane.apply.readInode(global);
    }

    pub fn dentry(self: *Holder, parent: u64, name: []const u8) !?applier.Found {
        return self.plane.apply.findDentry(parent, name);
    }

    /// The dentry AT ITS HOLDER (the divert's lookup stand-in).
    pub fn lookupAt(self: *Holder, parent: u64, name: []const u8) ops.Error!?applier.Found {
        const h: *Holder = if (self.store.leases(route_table.slotOf(parent))) self else self.other();
        return h.plane.apply.findDentry(parent, name) catch error.Io;
    }

    /// Seed a directory record in this holder's store.
    pub fn seedDir(self: *Holder, global: u64) !void {
        const t = try self.store.begin(@src());
        try t.stagePut(key.TREE_INODES, &key.inodeKey(route_table.effectiveIno(global)), &(InodeValue{ .mode = S.IFDIR | 0o755, .nlink = 2, .mtime = 1, .ctime = 1 }).encode());
        try self.store.commit(t);
    }

    pub fn seedFile(self: *Holder, global: u64, nlink: u32) !void {
        const t = try self.store.begin(@src());
        try t.stagePut(key.TREE_INODES, &key.inodeKey(route_table.effectiveIno(global)), &(InodeValue{ .mode = S.IFREG | 0o644, .nlink = nlink, .mtime = 1, .ctime = 1 }).encode());
        try self.store.commit(t);
    }

    /// Run `plan` as the product does: the travelling guards over the
    /// plan's keys, the execute, the release.
    pub fn run(self: *Holder, plan: *const steps.Plan) ops.Error!ops.Done {
        const scope = self.plane.mintScope();
        var held = try self.plane.acquirePlanGuards(plan, scope, false);
        defer held.release(self.set.gpa);
        return self.plane.execute(plan, scope, 1_000);
    }

    pub fn create(self: *Holder, parent: u64, name: []const u8, mode: u32) ops.Error!u64 {
        const child = self.mint();
        const plan = steps.planCreate(.{ .parent = parent, .name = name, .child = child, .mode = mode, .uid = 1, .gid = 2, .now = 1_000 });
        const done = try self.run(&plan);
        if (done.refused) |_| return error.Refused;
        return child;
    }

    pub fn unlink(self: *Holder, parent: u64, name: []const u8, is_dir: bool) ops.Error!void {
        const f = (try self.lookupAt(parent, name)) orelse return error.Corrupt;
        const cv = try self.plane.readWitness(f.child);
        const plan = steps.planUnlink(.{ .parent = parent, .name = name, .child = f.child, .child_value = cv, .is_dir = is_dir, .now = 1_000 });
        const done = try self.run(&plan);
        if (done.refused) |_| return error.Refused;
    }

    pub fn link(self: *Holder, parent: u64, name: []const u8, ino: u64) ops.Error!void {
        const cv = (try self.plane.readWitness(ino)) orelse return error.Corrupt;
        const plan = steps.planLink(.{ .parent = parent, .name = name, .ino = ino, .child_value = cv, .now = 1_000 }) catch return error.Refused;
        const done = try self.run(&plan);
        if (done.refused) |_| return error.Refused;
    }

    pub fn rename(self: *Holder, old_parent: u64, old_name: []const u8, new_parent: u64, new_name: []const u8) ops.Error!void {
        const f = (try self.lookupAt(old_parent, old_name)) orelse return error.Corrupt;
        const cv = (try self.plane.readWitness(f.child)) orelse return error.Corrupt;
        var dest: ?struct { ino: u64, value: InodeValue } = null;
        if (try self.lookupAt(new_parent, new_name)) |d| {
            dest = .{ .ino = d.child, .value = (try self.plane.readWitness(d.child)) orelse return error.Corrupt };
        }
        const plan = steps.planRename(.{ .old_parent = old_parent, .old_name = old_name, .new_parent = new_parent, .new_name = new_name, .child = f.child, .child_mode = cv.mode, .dest = if (dest) |d| .{ .ino = d.ino, .value = d.value } else null, .now = 1_000 });
        const done = try self.run(&plan);
        if (done.refused) |_| return error.Refused;
    }
};

pub const TwoHolders = struct {
    gpa: Allocator,
    io: Io,
    hub: loopback.LoopbackHub,
    a: Holder,
    b: Holder,
    control: RamControl = .{},
    lease: dir_rename.Lease,
    lease_svc: dir_rename.LeaseService,
    refreshes: u32 = 0,

    pub const A_SLOTS = [_]u16{ 1, 0 };
    pub const B_SLOTS = [_]u16{ 3, 4 };

    fn identityOf(n: u64) verbs.WireIdentity {
        return .{ .node_token = 0x1000 + n, .writer_id = 0x2000 + n, .mount_slot = @intCast(n), .reserved = 0 };
    }

    pub fn init(gpa: Allocator, io: Io, seams_a: ops.Seams, seams_b: ops.Seams) !*TwoHolders {
        const self = try gpa.create(TwoHolders);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .io = io, .hub = loopback.LoopbackHub.init(gpa, io), .a = undefined, .b = undefined, .lease = undefined, .lease_svc = undefined };
        try self.initHolder(&self.a, 1, &A_SLOTS, seams_a);
        try self.initHolder(&self.b, 2, &B_SLOTS, seams_b);
        self.lease = .{ .io = io, .store = self.control.store(), .term = 1, .own_id = 1 };
        self.lease_svc = .{ .lease = &self.lease };
        self.lease_svc.register(&self.a.svc);
        const secret: auth.Secret = @splat(0x33);
        self.a.daemon = try self.hub.addDaemon(&self.a.svc, secret, identityOf(1));
        self.b.daemon = try self.hub.addDaemon(&self.b.svc, secret, identityOf(2));
        self.a.conn = try self.hub.connect(self.a.daemon, self.b.daemon);
        self.b.conn = try self.hub.connect(self.b.daemon, self.a.daemon);
        return self;
    }

    fn initHolder(self: *TwoHolders, h: *Holder, id: u32, slots_: []const u16, seams: ops.Seams) !void {
        h.* = .{ .set = self, .id = id, .store = .{ .mem = try kv_fs.MemStore.init(self.gpa) }, .locks = undefined, .plane = undefined, .step_svc = undefined, .guards_svc = undefined };
        try h.store.leased.appendSlice(self.gpa, slots_);
        h.locks = try dlm.Manager.init(self.gpa, 64);
        h.plane = try Plane.init(self.gpa, self.io, &h.store, &h.locks, id, 0, 7, h.resolver(), seams, 16 << 10);
        h.plane.witness = h.witness();
        h.step_svc = .{ .plane = &h.plane };
        h.step_svc.register(&h.svc);
        h.guards_svc = .{ .parked = &h.plane.parked, .volume = 0 };
        h.guards_svc.register(&h.svc);
    }

    /// A "remount" of holder `h`: a fresh plane over the SAME store (the
    /// register and the minter start empty, as a new incarnation's do).
    pub fn remount(self: *TwoHolders, h: *Holder, seams: ops.Seams) !void {
        h.plane.deinit();
        h.plane = try Plane.init(self.gpa, self.io, &h.store, &h.locks, h.id, 0, 8, h.resolver(), seams, 16 << 10);
        h.plane.witness = h.witness();
        h.step_svc.plane = &h.plane;
        h.guards_svc.parked = &h.plane.parked;
    }

    pub fn deinit(self: *TwoHolders) void {
        self.a.conn.deinit();
        self.b.conn.deinit();
        for ([_]*Holder{ &self.a, &self.b }) |h| {
            h.plane.deinit();
            h.locks.deinit();
            h.store.leased.deinit(self.gpa);
            h.store.mem.deinit();
        }
        self.hub.deinit();
        self.gpa.destroy(self);
    }

    /// The per-process closure: `minted ≡ retired + open`.
    pub fn closureHolds(_: *TwoHolders, minted0: u64, retired0: u64, open0: u64) !void {
        const minted = intent.stats.xv_cross_owner_intents_minted.load() - minted0;
        const retired = intent.stats.xv_cross_owner_intents_retired.load() - retired0;
        const open = intent.stats.xv_cross_owner_intents_open.load() - open0;
        try testing.expectEqual(minted, retired + open);
    }

    /// Every intent home of both holders is empty.
    pub fn noOpenIntents(self: *TwoHolders) !void {
        for ([_]*Holder{ &self.a, &self.b }) |h| {
            for (h.store.leased.items) |s| {
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(self.gpa, &kvs);
                try h.plane.apply.scanHome(intent.homeIno(s), &kvs);
                try testing.expectEqual(@as(usize, 0), kvs.items.len);
            }
            try testing.expectEqual(@as(usize, 0), h.plane.register.count());
        }
    }
};

const Snapshot = struct {
    minted: u64,
    retired: u64,
    open: u64,
    shipped: u64,
    served: u64,
    fn take() Snapshot {
        return .{ .minted = intent.stats.xv_cross_owner_intents_minted.load(), .retired = intent.stats.xv_cross_owner_intents_retired.load(), .open = intent.stats.xv_cross_owner_intents_open.load(), .shipped = ops.stats.xv_cross_owner_steps_shipped.load(), .served = ops.stats.xv_cross_owner_steps_served.load() };
    }
};

/// B's directory (slot 3) and A's directory (slot 1).
const DIR_B: u64 = gino(2, 3);
const DIR_A: u64 = gino(2, 1);

test "a create in a FOREIGN directory ships ONE InsertDentry and mints the child in the creator's rotor: step 0 = the record + the intent in A's ring, step 1 shipped to B; the name lands at B, the record at A; one intent minted and retired, shipped ≡ served, one guard round trip to B's table" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    const s0 = Snapshot.take();
    const rpcs0 = guards.stats.xv_cross_owner_guard_rpcs.load();
    const child = try f.a.create(DIR_B, "f", S.IFREG | 0o644);
    try testing.expectEqual(@as(u16, 1), route_table.slotOf(child));
    try testing.expectEqual(child, (try f.b.dentry(DIR_B, "f")).?.child);
    try testing.expectEqual(@as(u32, 1), (try f.a.inode(child)).?.nlink);
    try testing.expect((try f.b.inode(child)) == null);
    const s1 = Snapshot.take();
    try testing.expectEqual(s0.minted + 1, s1.minted);
    try testing.expectEqual(s0.retired + 1, s1.retired);
    try testing.expectEqual(s0.shipped + 1, s1.shipped);
    try testing.expectEqual(s0.served + 1, s1.served);
    try testing.expectEqual(rpcs0 + 1, guards.stats.xv_cross_owner_guard_rpcs.load());
    try testing.expectEqual(@as(usize, 0), f.b.plane.parked.count());
    try f.closureHolds(s0.minted, s0.retired, s0.open);
    try f.noOpenIntents();
    // The exact-sum law of the phases.
    const sum = ops.stats.xv_cross_owner_phase_plan.snapshot().sum_ns + ops.stats.xv_cross_owner_phase_intent_barrier.snapshot().sum_ns + ops.stats.xv_cross_owner_phase_ship_rtt.snapshot().sum_ns + ops.stats.xv_cross_owner_phase_local_steps.snapshot().sum_ns + ops.stats.xv_cross_owner_phase_retire.snapshot().sum_ns;
    try testing.expectEqual(ops.stats.xv_cross_owner_phase_total.snapshot().sum_ns, sum);
    // A mkdir bumps the foreign parent's nlink at its holder.
    const sub = try f.a.create(DIR_B, "d", S.IFDIR | 0o755);
    try testing.expectEqual(@as(u32, 3), (try f.b.inode(DIR_B)).?.nlink);
    try testing.expectEqual(@as(u32, 2), (try f.a.inode(sub)).?.nlink);
}

test "unlink, rmdir, link, rename and exchange across two holders leave a byte-exact tree: every op ONE intent, every intent retired, shipped ≡ served, no open intent anywhere" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.a.seedDir(DIR_A);
    try f.b.seedDir(DIR_B);
    const s0 = Snapshot.take();
    // A creates two files and a directory in B's directory.
    const fa = try f.a.create(DIR_B, "fa", S.IFREG | 0o644);
    _ = try f.a.create(DIR_B, "fb", S.IFREG | 0o644);
    const da = try f.a.create(DIR_B, "da", S.IFDIR | 0o755);
    try testing.expectEqual(@as(u32, 3), (try f.b.inode(DIR_B)).?.nlink);
    // B links A's file into its own directory: the count at A, the name at B.
    try f.b.link(DIR_B, "fa2", fa);
    try testing.expectEqual(@as(u32, 2), (try f.a.inode(fa)).?.nlink);
    // A unlinks one name: nlink back to 1, the name gone at B.
    try f.a.unlink(DIR_B, "fa2", false);
    try testing.expectEqual(@as(u32, 1), (try f.a.inode(fa)).?.nlink);
    try testing.expect((try f.b.dentry(DIR_B, "fa2")) == null);
    // B unlinks A's other file (B's directory, A's child).
    try f.b.unlink(DIR_B, "fb", false);
    try testing.expect((try f.b.dentry(DIR_B, "fb")) == null);
    // A renames its file from B's directory into its own.
    try f.a.rename(DIR_B, "fa", DIR_A, "moved");
    try testing.expect((try f.b.dentry(DIR_B, "fa")) == null);
    try testing.expectEqual(fa, (try f.a.dentry(DIR_A, "moved")).?.child);
    try testing.expectEqual(@as(u64, 1_000), (try f.a.inode(fa)).?.ctime);
    // B renames the directory out of its own tree into A's: both parents'
    // nlink shift.
    try f.b.rename(DIR_B, "da", DIR_A, "db");
    try testing.expectEqual(@as(u32, 2), (try f.b.inode(DIR_B)).?.nlink);
    try testing.expectEqual(@as(u32, 3), (try f.a.inode(DIR_A)).?.nlink);
    try testing.expectEqual(da, (try f.a.dentry(DIR_A, "db")).?.child);
    // rmdir of the moved directory (A's child, A's directory — but B initiates).
    try f.b.unlink(DIR_A, "db", true);
    try testing.expectEqual(@as(u32, 0), (try f.a.inode(da)).?.nlink);
    try testing.expectEqual(@as(u32, 2), (try f.a.inode(DIR_A)).?.nlink);
    // An exchange across holders: A's "moved" ↔ a file B creates in its dir.
    const fb2 = try f.b.create(DIR_B, "x", S.IFREG | 0o644);
    const plan = steps.planExchange(.{ .old_parent = DIR_A, .old_name = "moved", .new_parent = DIR_B, .new_name = "x", .a = fa, .a_mode = S.IFREG, .b = fb2, .b_mode = S.IFREG, .now = 2_000 });
    const done = try f.a.run(&plan);
    try testing.expect(done.refused == null);
    try testing.expectEqual(fb2, (try f.a.dentry(DIR_A, "moved")).?.child);
    try testing.expectEqual(fa, (try f.b.dentry(DIR_B, "x")).?.child);
    try testing.expectEqual(@as(u64, 2_000), (try f.a.inode(fa)).?.ctime);
    const s1 = Snapshot.take();
    try testing.expectEqual(@as(u64, 11), s1.minted - s0.minted);
    try testing.expectEqual(s1.minted, s1.retired - s0.retired + s0.minted);
    try testing.expectEqual(s1.shipped - s0.shipped, s1.served - s0.served);
    try f.noOpenIntents();
}

test "a ship that fails leaves the intent OPEN (abandoned on the register, never a fail-stop); the roll-forward cadence completes it: the name lands exactly once and the intent retires" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{ .ship_fail_at = 1 }, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    const s0 = Snapshot.take();
    const abandoned0 = ops.stats.xv_abandoned.load();
    try testing.expectError(error.ShipFailed, f.a.create(DIR_B, "f", S.IFREG | 0o644));
    try testing.expectEqual(abandoned0 + 1, ops.stats.xv_abandoned.load());
    try testing.expectEqual(@as(usize, 1), f.a.plane.register.count());
    try testing.expect(f.a.plane.register.anyAbandoned());
    try testing.expect((try f.b.dentry(DIR_B, "f")) == null);
    try testing.expectEqual(s0.open + 1, intent.stats.xv_cross_owner_intents_open.load());
    // The cadence: the pass rolls the plan forward (the ship now lands).
    const recovered0 = ops.stats.xv_recovered.load();
    try testing.expectEqual(@as(u32, 1), try f.a.plane.cadenceTick(clock.monotonicNs()));
    try testing.expectEqual(recovered0 + 1, ops.stats.xv_recovered.load());
    try testing.expect((try f.b.dentry(DIR_B, "f")) != null);
    try f.closureHolds(s0.minted, s0.retired, s0.open);
    try f.noOpenIntents();
    // An idle cadence runs no pass.
    const passes = ops.stats.xv_cadence_passes.load();
    try testing.expectEqual(@as(u32, 0), try f.a.plane.cadenceTick(clock.monotonicNs()));
    try testing.expectEqual(passes, ops.stats.xv_cadence_passes.load());
}

test "an abandoned create whose name a retry took meanwhile is COMPENSATED by the roll-forward (found by the shared hot directory row: fsck C9 found a live inode named by no dentry after kill -9 of a stripes' holder — a create's ship failed against the corpse, the shell retried the SAME name and won it after the recovery, and the roll-forward skipped the refused insert and retired, orphaning the first attempt's child): the minted child destroyed, the intent retired, the retry's name intact, counted" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{ .ship_fail_at = 1 }, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    const s0 = Snapshot.take();
    // The first attempt: the child minted at A, the insert's ship fails —
    // the intent stays open, the caller answered EAGAIN.
    const first = f.a.mint();
    const plan = steps.planCreate(.{ .parent = DIR_B, .name = "f", .child = first, .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
    try testing.expectError(error.ShipFailed, f.a.run(&plan));
    try testing.expect((try f.a.inode(first)) != null);
    try testing.expect(f.a.plane.register.anyAbandoned());
    // The caller's retry of the same name lands with a NEW child.
    const second = try f.a.create(DIR_B, "f", S.IFREG | 0o644);
    try testing.expectEqual(second, (try f.b.dentry(DIR_B, "f")).?.child);
    // The roll-forward re-meets the refusal: it compensates instead of
    // skipping — the first child goes, the intent retires.
    const comp0 = ops.stats.xv_recovered_compensated.load();
    try testing.expectEqual(@as(u32, 1), try f.a.plane.cadenceTick(clock.monotonicNs()));
    try testing.expectEqual(comp0 + 1, ops.stats.xv_recovered_compensated.load());
    try testing.expect((try f.a.inode(first)) == null);
    try testing.expectEqual(second, (try f.b.dentry(DIR_B, "f")).?.child);
    try testing.expect((try f.a.inode(second)) != null);
    try f.noOpenIntents();
    try f.closureHolds(s0.minted, s0.retired, s0.open);
}

test "an abandoned LINK whose name another object took meanwhile is compensated by the roll-forward INCLUDING its count (owed by the shared hot directory row's fix 10: the undo attributed only fresh mints, so the abandoned link's nlink step stood — the C10 class): the abandonment attests the steps it applied in the record, the roll-forward never re-witnesses them and inverts the count RELATIVE to what it finds (a later link of the same file moved it on) — nlink equals the names, in the same incarnation's cadence and after a remount" {
    const gpa = testing.allocator;
    for ([_]bool{ false, true }) |remount| {
        const f = try TwoHolders.init(gpa, testing.io, .{ .ship_fail_at = 1 }, .{});
        defer f.deinit();
        try f.a.seedDir(DIR_A);
        try f.b.seedDir(DIR_B);
        const s0 = Snapshot.take();
        const file = try f.a.create(DIR_A, "orig", S.IFREG | 0o644);
        // The first attempt: the count at A (+1), the name's ship to B
        // fails — the intent stays open, the caller answered EAGAIN.
        const attested0 = ops.stats.xv_applied_attested.load();
        try testing.expectError(error.ShipFailed, f.a.link(DIR_B, "l", file));
        try testing.expectEqual(attested0 + 1, ops.stats.xv_applied_attested.load());
        try testing.expectEqual(@as(u32, 2), (try f.a.inode(file)).?.nlink);
        try testing.expect((try f.b.dentry(DIR_B, "l")) == null);
        // Meanwhile: the file is linked again elsewhere (count 3), and a
        // NEW file takes the name the open intent wanted.
        try f.a.link(DIR_A, "again", file);
        try testing.expectEqual(@as(u32, 3), (try f.a.inode(file)).?.nlink);
        const other = try f.a.create(DIR_B, "l", S.IFREG | 0o644);
        // The roll-forward re-meets the refused name and compensates the
        // count its abandonment attested: one count per name.
        const comp0 = ops.stats.xv_recovered_compensated.load();
        if (remount) {
            try f.remount(&f.a, .{});
            try testing.expectEqual(@as(u32, 1), try f.a.plane.recoverOpen(clock.monotonicNs()));
        } else {
            try testing.expectEqual(@as(u32, 1), try f.a.plane.cadenceTick(clock.monotonicNs()));
        }
        try testing.expectEqual(comp0 + 1, ops.stats.xv_recovered_compensated.load());
        try testing.expectEqual(@as(u32, 2), (try f.a.inode(file)).?.nlink);
        try testing.expectEqual(file, (try f.a.dentry(DIR_A, "orig")).?.child);
        try testing.expectEqual(file, (try f.a.dentry(DIR_A, "again")).?.child);
        try testing.expectEqual(other, (try f.b.dentry(DIR_B, "l")).?.child);
        try testing.expectEqual(@as(u32, 1), (try f.a.inode(other)).?.nlink);
        try f.noOpenIntents();
        try f.closureHolds(s0.minted, s0.retired, s0.open);
    }
}

test "every initiator crash window of a foreign create rolls forward to exactly ONE outcome: after step 0 (+ the intent), after the foreign step, before the retirement — the next mount's recovery completes the plan and retires; recovery is idempotent across repeated mounts" {
    const gpa = testing.allocator;
    for ([_]u32{ 0, 1, 2 }) |window| {
        const f = try TwoHolders.init(gpa, testing.io, .{ .crash_after_step = window }, .{});
        defer f.deinit();
        try f.b.seedDir(DIR_B);
        const s0 = Snapshot.take();
        try testing.expectError(error.Crashed, f.a.create(DIR_B, "f", S.IFREG | 0o644));
        // The record is durable in A's ring whatever the window.
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        try f.a.plane.apply.scanHome(intent.homeIno(1), &kvs);
        try testing.expectEqual(@as(usize, 1), kvs.items.len);
        const named_before = (try f.b.dentry(DIR_B, "f")) != null;
        try testing.expectEqual(window >= 1, named_before);
        // "kill -9": the next incarnation of A recovers before it serves.
        try f.remount(&f.a, .{});
        try testing.expectEqual(@as(u32, 1), try f.a.plane.recoverOpen(2_000));
        const found = (try f.b.dentry(DIR_B, "f")).?;
        try testing.expect((try f.a.inode(found.child)) != null);
        try f.noOpenIntents();
        // Exactly one name, one record: a second mount recovers nothing.
        try testing.expectEqual(@as(u32, 0), try f.a.plane.recoverOpen(3_000));
        try f.closureHolds(s0.minted, s0.retired, s0.open);
    }
}

test "a shipped step refused SlotMoved at its holder is re-dispatched ONCE after a re-resolve (the manager's word), lands, and the plan completes" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{ .serve_slot_moved_once = true });
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    const retries0 = ops.stats.xv_cross_owner_step_slot_moved_retries.load();
    _ = try f.a.create(DIR_B, "f", S.IFREG | 0o644);
    try testing.expect((try f.b.dentry(DIR_B, "f")) != null);
    try testing.expectEqual(retries0 + 1, ops.stats.xv_cross_owner_step_slot_moved_retries.load());
    try testing.expectEqual(@as(u32, 1), f.refreshes);
    try f.noOpenIntents();
}

test "a holder that cannot be reached is re-resolved before the ship is given up (found by the shared hot directory row: a creator dialled the killed holder of a stripe the manager had recovered until its intents stood past the TTL): the step lands at the holder the re-resolve names, in the same plan; a slot still unreachable after the retries leaves the intent open, counted abandoned" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    // The plan's guards travel under a live view; the step's own
    // resolution then meets the dead holder.
    {
        const plan = steps.planCreate(.{ .parent = DIR_B, .name = "f", .child = f.a.mint(), .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
        const scope = f.a.plane.mintScope();
        var held = try f.a.plane.acquirePlanGuards(&plan, scope, false);
        defer held.release(gpa);
        f.a.unreachable_once = 3;
        const refreshes0 = f.refreshes;
        const done = try f.a.plane.execute(&plan, scope, 1_000);
        try testing.expect(done.refused == null);
        try testing.expectEqual(refreshes0 + 1, f.refreshes);
    }
    try testing.expect((try f.b.dentry(DIR_B, "f")) != null);
    try f.noOpenIntents();
    // Nobody leases the slot through every re-resolve: the ship is given
    // up after the retries, the intent left open for the roll-forward.
    const abandoned0 = ops.stats.xv_abandoned.load();
    {
        const plan = steps.planCreate(.{ .parent = DIR_B, .name = "g", .child = f.a.mint(), .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
        const scope = f.a.plane.mintScope();
        var held = try f.a.plane.acquirePlanGuards(&plan, scope, false);
        defer held.release(gpa);
        const i = std.mem.indexOfScalar(u16, f.b.store.leased.items, 3).?;
        _ = f.b.store.leased.swapRemove(i);
        const refreshes1 = f.refreshes;
        try testing.expectError(error.ShipFailed, f.a.plane.execute(&plan, scope, 1_000));
        try testing.expectEqual(refreshes1 + ops.SLOT_MOVED_RETRIES, f.refreshes);
    }
    try testing.expectEqual(abandoned0 + 1, ops.stats.xv_abandoned.load());
    try testing.expect(f.a.plane.register.anyAbandoned());
    // The slot's holder is back (the manager re-leased it): the roll-
    // forward lands the open intent there.
    try f.b.store.leased.append(gpa, 3);
    try testing.expectEqual(@as(u32, 1), try f.a.plane.cadenceTick(clock.monotonicNs()));
    try testing.expect((try f.b.dentry(DIR_B, "g")) != null);
    try f.noOpenIntents();
}

test "the plan's guards meeting a DEAD view of a key's holder re-resolve the slot before they are given up (decision 0003 §X.8; found by the shared hot directory row under the move gate: C's create into a stripe of the killed B asked its guards of the dead holder for the lattice's whole deadline, 48 s, then answered EIO): the guards travel to the holder the re-resolve names, the create lands in the same attempt; a slot nobody leases through the retries answers the move class with every retry re-resolved" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    const reresolves0 = guards.stats.xv_cross_owner_guard_stale_reresolves.load();
    {
        const plan = steps.planCreate(.{ .parent = DIR_B, .name = "f", .child = f.a.mint(), .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
        const scope = f.a.plane.mintScope();
        f.a.unreachable_once = 3;
        const refreshes0 = f.refreshes;
        var held = try f.a.plane.acquirePlanGuards(&plan, scope, false);
        defer held.release(gpa);
        try testing.expectEqual(refreshes0 + 1, f.refreshes);
        try testing.expectEqual(reresolves0 + 1, guards.stats.xv_cross_owner_guard_stale_reresolves.load());
        const done = try f.a.plane.execute(&plan, scope, 1_000);
        try testing.expect(done.refused == null);
    }
    try testing.expect((try f.b.dentry(DIR_B, "f")) != null);
    try f.noOpenIntents();
    // Nobody leases the slot: every retry re-resolves, then the move class.
    const i = std.mem.indexOfScalar(u16, f.b.store.leased.items, 3).?;
    _ = f.b.store.leased.swapRemove(i);
    {
        const plan = steps.planCreate(.{ .parent = DIR_B, .name = "g", .child = f.a.mint(), .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
        const refreshes1 = f.refreshes;
        try testing.expectError(error.ShipFailed, f.a.plane.acquirePlanGuards(&plan, f.a.plane.mintScope(), false));
        try testing.expectEqual(refreshes1 + ops.SLOT_MOVED_RETRIES, f.refreshes);
        try testing.expect(errors.isMoveClass(ops.coreError(error.ShipFailed)));
    }
    try f.b.store.leased.append(gpa, 3);
}

/// A's create of `name` in B's directory with its insert meeting slot 3
/// MID-MOVE (nobody leases it once the guards travelled): the execute
/// fails and, held, names the intent it left open.
fn createMidMove(f: *TwoHolders, name: []const u8, child: u64) !ops.HeldOpen {
    const plan = steps.planCreate(.{ .parent = DIR_B, .name = name, .child = child, .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
    const scope = f.a.plane.mintScope();
    var guards_held = try f.a.plane.acquirePlanGuards(&plan, scope, false);
    defer guards_held.release(f.gpa);
    const i = std.mem.indexOfScalar(u16, f.b.store.leased.items, 3).?;
    _ = f.b.store.leased.swapRemove(i);
    var left: ?ops.HeldOpen = null;
    try testing.expectError(error.ShipFailed, f.a.plane.executeHeld(&plan, scope, 1_000, &left));
    return left orelse error.TestNotHeld;
}

test "a create whose insert met a slot MID-MOVE stays the LIVE op's (decision 0003 §X.2): the intent held in flight — the cadence never takes it while the op may resume — and the op's re-issue resumes THAT intent once the move lands: the same child, one name, no orphan, the closure; a resume that finds the name taken meanwhile compensates (the first attempt's child destroyed) and answers EEXIST; a give-up hands the held intent to the cadence" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    const s0 = Snapshot.take();
    const held0 = ops.stats.xv_held_open.load();
    // The re-issue resumes: the same child, named once.
    const child = f.a.mint();
    const open = try createMidMove(f, "x", child);
    try testing.expectEqual(held0 + 1, ops.stats.xv_held_open.load());
    try testing.expectEqual(intent.IntentState.in_flight, f.a.plane.register.stateOf(open.tx_id).?);
    try testing.expect(!f.a.plane.register.anyAbandoned());
    try testing.expectEqual(@as(u32, 0), try f.a.plane.cadenceTick(clock.monotonicNs()));
    try testing.expect((try f.a.inode(child)) != null);
    try f.b.store.leased.append(gpa, 3);
    var again: ?ops.HeldOpen = null;
    const done = try f.a.plane.resumeHeld(open, 1_000, &again);
    try testing.expect(done.refused == null);
    try testing.expect(again == null);
    try testing.expectEqual(child, (try f.b.dentry(DIR_B, "x")).?.child);
    try testing.expectEqual(@as(u32, 1), (try f.a.inode(child)).?.nlink);
    try f.noOpenIntents();
    // A resume that meets the name taken meanwhile: compensated, EEXIST.
    const child2 = f.a.mint();
    const open2 = try createMidMove(f, "y", child2);
    try f.b.store.leased.append(gpa, 3);
    const other = try f.b.create(DIR_B, "y", S.IFREG | 0o644);
    const destroyed0 = applier.stats.xv_cross_owner_mints_destroyed.load();
    const done2 = try f.a.plane.resumeHeld(open2, 1_000, &again);
    try testing.expectEqual(errors.Error.PathAlreadyExists, done2.refused.?);
    try testing.expectEqual(destroyed0 + 1, applier.stats.xv_cross_owner_mints_destroyed.load());
    try testing.expect((try f.a.inode(child2)) == null);
    try testing.expectEqual(other, (try f.b.dentry(DIR_B, "y")).?.child);
    try f.noOpenIntents();
    // The op gives up (the gate's deadline): the cadence completes it.
    const child3 = f.a.mint();
    const open3 = try createMidMove(f, "z", child3);
    try f.b.store.leased.append(gpa, 3);
    f.a.plane.abandonHeld(open3.tx_id);
    try testing.expectEqual(@as(u32, 1), try f.a.plane.cadenceTick(clock.monotonicNs()));
    try testing.expectEqual(child3, (try f.b.dentry(DIR_B, "z")).?.child);
    try f.noOpenIntents();
    try f.closureHolds(s0.minted, s0.retired, s0.open);
}

test "a live witness refusal at the shipped step (a racing creator's name landed first) COMPENSATES: the ABORTING flip, the minted child destroyed, the intent retired, EEXIST answered; and an ABORTING intent met by the roll-forward runs the INVERSE plan" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    const other = try f.b.create(DIR_B, "f", S.IFREG | 0o644);
    const s0 = Snapshot.take();
    const comp0 = ops.stats.xv_compensated.load();
    const destroyed0 = applier.stats.xv_cross_owner_mints_destroyed.load();
    const child = f.a.mint();
    const plan = steps.planCreate(.{ .parent = DIR_B, .name = "f", .child = child, .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
    const done = try f.a.run(&plan);
    try testing.expectEqual(errors.Error.PathAlreadyExists, done.refused.?);
    try testing.expectEqual(comp0 + 1, ops.stats.xv_compensated.load());
    try testing.expectEqual(destroyed0 + 1, applier.stats.xv_cross_owner_mints_destroyed.load());
    try testing.expect((try f.a.inode(child)) == null);
    try testing.expectEqual(other, (try f.b.dentry(DIR_B, "f")).?.child);
    try f.closureHolds(s0.minted, s0.retired, s0.open);
    try f.noOpenIntents();
    // An ABORTING record left by a crash between the flip and the last
    // inverse: the roll-forward inverts (the minted child goes) and retires.
    const child2 = f.a.mint();
    const plan2 = steps.planCreate(.{ .parent = DIR_B, .name = "g", .child = child2, .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
    _ = try f.a.plane.apply.apply(plan2.steps[0], .none, .none, 1_000);
    var recs: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
    var img_buf: [4096]u8 = undefined;
    const img = try intent.encode(.create, intent.FLAG_ABORTING, (7 << 40) | 500, 0, plan2.recs(&recs), 16 << 10, &img_buf);
    try f.a.plane.apply.commitRiderAlone(.{ .put = .{ .home = intent.homeIno(1), .tx_id = (7 << 40) | 500, .image = img } }, .none);
    try testing.expect((try f.a.inode(child2)) != null);
    try f.remount(&f.a, .{});
    try testing.expectEqual(@as(u32, 1), try f.a.plane.recoverOpen(2_000));
    try testing.expect((try f.a.inode(child2)) == null);
    try testing.expect((try f.b.dentry(DIR_B, "g")) == null);
    try f.noOpenIntents();
}

test "the dir_rename lease serializes two directory renames of two holders over the wire (the loser waits on Busy, proceeds at the unlock); the exact ancestry walk through LookupExact at each ancestor's holder refuses a rename into the moved directory's own subtree across holders" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    // A's taker is in-process; B's rides the wire to A (volume 0's manager).
    var ta: dir_rename.Taker = .{ .io = testing.io, .appender_id = 1, .manager = .{ .local = &f.lease } };
    var tb: dir_rename.Taker = .{ .io = testing.io, .appender_id = 2, .manager = .{ .remote = linkOf(&f.b.conn) } };
    var held_a = try ta.take(std.math.maxInt(u64));
    try testing.expectEqual(@as(u32, 1), (try f.lease.holder()).?.holder);
    try testing.expectError(error.Deferred, tb.take(clock.monotonicNs() + 20 * std.time.ns_per_ms));
    try testing.expect(tb.busy_waits.load(.monotonic) >= 1);
    held_a.release();
    var held_b = try tb.take(std.math.maxInt(u64));
    try testing.expectEqual(@as(u32, 2), (try f.lease.holder()).?.holder);
    held_b.release();
    try testing.expect((try f.lease.holder()) == null);
    // The ancestry across holders: / ─ DIR_B (B) ─ sub (A's mint, A's slot)
    // ─ leaf (B's mint). Moving DIR_B into leaf must refuse.
    try f.b.seedDir(DIR_B);
    const sub = try f.a.create(DIR_B, "sub", S.IFDIR | 0o755);
    const leaf = try f.b.create(sub, "leaf", S.IFDIR | 0o755);
    const Memo = struct {
        f: *TwoHolders,
        /// The memo every lookup fills: the chain as the test built it
        /// (`DIR_B` under the root; `sub` = A's first mint; `leaf` = B's).
        fn linkOf_(_: *anyopaque, dir: u64, name_buf: *[255]u8) ?dir_rename.ParentLink {
            const DIRB = gino(2, 3);
            const sub_ = gino(100, 1);
            const leaf_ = gino(100, 3);
            if (dir == DIRB) {
                @memcpy(name_buf[0..4], "dirb");
                return .{ .parent = inos.ROOT_INO, .name = name_buf[0..4] };
            }
            if (dir == sub_) {
                @memcpy(name_buf[0..3], "sub");
                return .{ .parent = DIRB, .name = name_buf[0..3] };
            }
            if (dir == leaf_) {
                @memcpy(name_buf[0..4], "leaf");
                return .{ .parent = sub_, .name = name_buf[0..4] };
            }
            return null;
        }
        fn scan(_: *anyopaque, _: u64, _: *[255]u8) ?dir_rename.ParentLink {
            return null;
        }
        fn exact(ctx: *anyopaque, parent: u64, name: []const u8) errors.Error!?u64 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            // The root is A's; every other parent at its holder.
            if (parent == inos.ROOT_INO) return if (std.mem.eql(u8, name, "dirb")) gino(2, 3) else null;
            const holder: *Holder = if (self.f.a.store.leases(route_table.slotOf(parent))) &self.f.a else &self.f.b;
            // A exact-looks-up B's directory over the wire; its own locally.
            if (holder == &self.f.b) return ops.lookupExactRemote(linkOf(&self.f.a.conn), 0, parent, name);
            const found = (holder.plane.lookupExact(parent, name) catch return error.InputOutput) orelse return null;
            return found.child;
        }
        fn stripe(_: *anyopaque, _: u64) ?u64 {
            return null;
        }
    };
    var memo: Memo = .{ .f = f };
    const anc: dir_rename.Ancestry = .{ .ctx = &memo, .linkOf = &Memo.linkOf_, .scanParent = &Memo.scan, .lookupExact = &Memo.exact, .stripeOwner = &Memo.stripe };
    try testing.expectEqual(sub, gino(100, 1));
    try testing.expectEqual(leaf, gino(100, 3));
    try testing.expectError(error.IntoOwnSubtree, dir_rename.refuseRenameIntoOwnSubtree(anc, DIR_B, leaf));
    try testing.expectError(error.IntoOwnSubtree, dir_rename.refuseRenameIntoOwnSubtree(anc, sub, leaf));
    try dir_rename.refuseRenameIntoOwnSubtree(anc, leaf, DIR_B);
    try dir_rename.refuseRenameIntoOwnSubtree(anc, leaf, inos.ROOT_INO);
}

test "the travelling guards: an op whose parent is foreign parks ONE scope at the holder's table (one guard RPC), a served step under the scope applies COVERED (takes nothing), the release at the end drops the scope" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    const child = f.a.mint();
    const plan = steps.planCreate(.{ .parent = DIR_B, .name = "f", .child = child, .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
    const scope = f.a.plane.mintScope();
    const rpcs0 = guards.stats.xv_cross_owner_guard_rpcs.load();
    const covered0 = guards.stats.xv_cross_owner_steps_covered.load();
    var held = try f.a.plane.acquirePlanGuards(&plan, scope, false);
    try testing.expectEqual(rpcs0 + 1, guards.stats.xv_cross_owner_guard_rpcs.load());
    try testing.expect(f.b.plane.parked.isParked(scope));
    try testing.expectEqual(@as(usize, 1), held.remoteCount());
    // B's own table holds the parent's stripe for A: a local acquisition
    // at B on the same key parks (the try form refuses).
    try testing.expectEqual(@as(?dlm.GuardSet, null), try f.b.locks.tryLockMany(testing.io, &.{.{ .ino = DIR_B, .mode = .exclusive }}, &.{}));
    const done = try f.a.plane.execute(&plan, scope, 1_000);
    try testing.expect(done.refused == null);
    try testing.expectEqual(covered0 + 1, guards.stats.xv_cross_owner_steps_covered.load());
    held.release(gpa);
    try testing.expect(!f.b.plane.parked.isParked(scope));
    var s = (try f.b.locks.tryLockMany(testing.io, &.{.{ .ino = DIR_B, .mode = .exclusive }}, &.{})).?;
    s.release(testing.io);
    // A served step with NO scope takes its own guards.
    const took0 = guards.stats.xv_cross_owner_steps_took_guards.load();
    const plan2 = steps.planCreate(.{ .parent = DIR_B, .name = "g", .child = f.a.mint(), .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 });
    _ = try f.a.plane.execute(&plan2, 0, 1_000);
    try testing.expectEqual(took0 + 1, guards.stats.xv_cross_owner_steps_took_guards.load());
}

/// A's ask for B's table while a local op at B holds the stripe: the
/// initiator's retry loop on its own thread.
const ContendedAsk = struct {
    f: *TwoHolders,
    plan: steps.Plan,
    scope: guards.Scope,
    held: ?guards.Travelling = null,
    err: ?ops.Error = null,
    fn run(self: *ContendedAsk) void {
        self.held = self.f.a.plane.acquirePlanGuards(&self.plan, self.scope, false) catch |e| {
            self.err = e;
            return;
        };
    }
};

test "the served acquisition never parks the holder's session: an ask on a stripe a local op at B holds answers Busy (nothing taken, no scope parked), the initiator retries on its own thread with the backoff, B's session serves another call meanwhile, and the ask lands once the stripe is freed; a step the scope does not cover meets the same law" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    // B's own op holds the directory's stripe.
    var local = (try f.b.locks.tryLockMany(testing.io, &.{.{ .ino = DIR_B, .mode = .exclusive }}, &.{})).?;
    const contended0 = guards.stats.xv_cross_owner_guards_contended.load();
    const retries0 = guards.stats.xv_cross_owner_guard_retries.load();
    const refusals0 = guards.stats.xv_cross_owner_guard_contention_refusals.load();
    var ask: ContendedAsk = .{ .f = f, .plan = steps.planCreate(.{ .parent = DIR_B, .name = "h", .child = f.a.mint(), .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 }), .scope = f.a.plane.mintScope() };
    const t = try std.Thread.spawn(.{}, ContendedAsk.run, .{&ask});
    // The holder answered Busy at least once and parked nothing; its
    // session is free: another call lands while the ask is retrying.
    var spins: u32 = 0;
    while (guards.stats.xv_cross_owner_guards_contended.load() == contended0) : (spins += 1) {
        try testing.expect(spins < 10_000_000);
        std.Thread.yield() catch {};
    }
    try testing.expect(!f.b.plane.parked.isParked(ask.scope));
    try testing.expect((try f.a.plane.readWitness(DIR_B)) != null);
    // The stripe freed: the retry lands, the scope parks, the plan runs.
    local.release(testing.io);
    t.join();
    try testing.expect(ask.err == null);
    try testing.expect(f.b.plane.parked.isParked(ask.scope));
    try testing.expect(guards.stats.xv_cross_owner_guard_retries.load() > retries0);
    try testing.expectEqual(refusals0, guards.stats.xv_cross_owner_guard_contention_refusals.load());
    const done = try f.a.plane.execute(&ask.plan, ask.scope, 1_000);
    try testing.expect(done.refused == null);
    ask.held.?.release(gpa);
    try testing.expect(!f.b.plane.parked.isParked(ask.scope));
    // A scope-less step against a held stripe: Busy, retried, landed.
    var local2 = (try f.b.locks.tryLockMany(testing.io, &.{.{ .ino = DIR_B, .mode = .exclusive }}, &.{})).?;
    const Step = struct {
        f: *TwoHolders,
        plan: steps.Plan,
        done: ?ops.Done = null,
        err: ?ops.Error = null,
        fn run(self: *@This()) void {
            self.done = self.f.a.plane.execute(&self.plan, 0, 1_000) catch |e| {
                self.err = e;
                return;
            };
        }
    };
    var st: Step = .{ .f = f, .plan = steps.planCreate(.{ .parent = DIR_B, .name = "i", .child = f.a.mint(), .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .now = 1_000 }) };
    const contended1 = guards.stats.xv_cross_owner_guards_contended.load();
    const t2 = try std.Thread.spawn(.{}, Step.run, .{&st});
    spins = 0;
    while (guards.stats.xv_cross_owner_guards_contended.load() == contended1) : (spins += 1) {
        try testing.expect(spins < 10_000_000);
        std.Thread.yield() catch {};
    }
    local2.release(testing.io);
    t2.join();
    try testing.expect(st.err == null and st.done.?.refused == null);
    try testing.expectEqual(st.plan.steps[1].insert_dentry.child, (try f.b.dentry(DIR_B, "i")).?.child);
}

const RecordShipService = record_ship.RecordShipService(HolderStore);

/// The holder's own-cache + kernel view, recorded.
const HolderView = struct {
    attr_drops: u32 = 0,
    invals: u32 = 0,
    prunes: u32 = 0,
    last_ino: u64 = 0,
    last_off: i64 = 0,
    fn drop(ctx: *anyopaque, _: u64) void {
        @as(*HolderView, @ptrCast(@alignCast(ctx))).attr_drops += 1;
    }
    fn discard(_: *anyopaque, _: u64) record_ship.DiscardVerdict {
        return .none;
    }
    fn inval(ctx: *anyopaque, ino: u64, off: i64, _: i64) error{QueueFull}!void {
        const self: *HolderView = @ptrCast(@alignCast(ctx));
        self.invals += 1;
        self.last_ino = ino;
        self.last_off = off;
    }
    fn prune(ctx: *anyopaque, _: []const u64) error{QueueFull}!void {
        @as(*HolderView, @ptrCast(@alignCast(ctx))).prunes += 1;
    }
    fn sink(self: *HolderView) record_ship.ServedSink {
        return .{ .caches = .{ .ctx = self, .dropAttr = &drop, .discardLayout = &discard }, .kernel = .{ .ctx = self, .invalInode = &inval, .prune = &prune }, .caps = .{ .writeback_cache = true, .prune_capable = true } };
    }
};

/// The harness's publish face: the layout record staged verbatim (the
/// refs diff is the data plane's; the RAM holder has none), the proofs
/// counted — a publish naming blocks without a proof is what the plane's
/// face refuses, pinned by `data/plane.zig`.
const RamPublish = struct {
    staged: u32 = 0,
    proofs_seen: usize = 0,
    fn stage(ctx: *anyopaque, t: *tx.KvTx, key_ino: u64, coll_seq: u8, _: ?[]const u8, value: []const u8, proofs: []align(1) const verbs.DmaProofWire) record_ship.PublishFace.PublishError!void {
        const self: *RamPublish = @ptrCast(@alignCast(ctx));
        self.staged += 1;
        self.proofs_seen += proofs.len;
        const layout_mod = @import("../../../data/layout.zig");
        var buf: [64]u8 = undefined;
        const enc = (record.XattrValue{ .name = layout_mod.LAYOUT_XATTR_NAME, .value = value }).encodeInto(&buf) catch return error.Rejected;
        const seed: key.NameKey = .init(0x5EED_0F_7E57);
        t.stagePut(key.TREE_XATTRS, &key.xattrKey(key_ino, key.xattrNameHash56(layout_mod.LAYOUT_XATTR_NAME, &seed), coll_seq), enc) catch return error.OutOfMemory;
    }
    fn face(self: *RamPublish) record_ship.PublishFace {
        return .{ .ctx = self, .stage = &stage };
    }
};

test "record ship: a chmod/setxattr/removexattr/layout publish of a FOREIGN-slot file lands at its holder as ONE verb each, under the holder's door, and the holder invalidates its own attr cache and kernel view (INVAL_INODE then PRUNE); the publish carries its DMA proofs to the holder's face and is REJECTED where no face is armed; a local file ships nothing; a slot-moved answer redirects once; ships ≡ served" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    var view: HolderView = .{};
    var rs_b: RecordShipService = .{ .plane = &f.b.plane, .sink = view.sink() };
    rs_b.register(&f.b.svc);
    var ram_pub: RamPublish = .{};
    const pub_face = ram_pub.face();
    const file = gino(50, 3);
    try f.b.seedFile(file, 1);
    const ships0 = record_ship.stats.record_ships.load();
    const served0 = record_ship.stats.record_served.load();
    // chmod from A: shipped, applied at B, B's view invalidated attrs-only.
    const rep = (try record_ship.ship(f.a.plane.resolver, 0, file, .{ .setattr = .{ .mode = 0o600, .mtime = 777 } }, 0)).?;
    try testing.expectEqual(@as(u32, S.IFREG | 0o600), rep.inode.?.mode);
    try testing.expectEqual(@as(u32, S.IFREG | 0o600), (try f.b.inode(file)).?.mode);
    try testing.expectEqual(@as(u64, 777), (try f.b.inode(file)).?.mtime);
    try testing.expectEqual(@as(u32, 1), view.attr_drops);
    try testing.expectEqual(@as(u32, 1), view.invals);
    try testing.expectEqual(@as(i64, -1), view.last_off);
    try testing.expectEqual(file, view.last_ino);
    try testing.expectEqual(@as(u32, 1), view.prunes);
    // setxattr + removexattr: the record lands and leaves; an absent name
    // is ENODATA at the caller (no refusal fired).
    _ = (try record_ship.ship(f.a.plane.resolver, 0, file, .{ .setxattr = .{ .name = "user.a", .value = "v" } }, 0)).?;
    try testing.expect((try rs_b.findXattr(route_table.effectiveIno(file), "user.a")) != null);
    const r2 = (try record_ship.ship(f.a.plane.resolver, 0, file, .{ .removexattr = .{ .name = "user.a" } }, 0)).?;
    try testing.expect(r2.inode != null);
    const r3 = (try record_ship.ship(f.a.plane.resolver, 0, file, .{ .removexattr = .{ .name = "user.a" } }, 0)).?;
    try testing.expect(r3.inode == null);
    // The layout publish: REJECTED while the holder has no publish face
    // (a volume-less holder stages no refs — never a dangling record);
    // armed, it lands with its proofs, whole-inode invalidation, counted.
    const pub0 = record_ship.stats.foreign_publish_ships.load();
    const pubs0 = record_ship.stats.foreign_publish_served.load();
    const refused0 = record_ship.stats.foreign_publish_refusals.load();
    const proofs = [_]verbs.DmaProofWire{.{ .vol_tag = 11, .offset = 1 << 22, .len = 1 << 22, .completed_ns = 5 }};
    try testing.expectError(error.Rejected, record_ship.ship(f.a.plane.resolver, 0, file, .{ .publish_layout = .{ .value = "LAYOUT", .proofs = &proofs } }, 3));
    try testing.expectEqual(refused0 + 1, record_ship.stats.foreign_publish_refusals.load());
    rs_b.armPublish(&pub_face);
    _ = (try record_ship.ship(f.a.plane.resolver, 0, file, .{ .publish_layout = .{ .value = "LAYOUT", .proofs = &proofs } }, 3)).?;
    try testing.expectEqual(@as(i64, 0), view.last_off);
    try testing.expectEqual(@as(u32, 1), ram_pub.staged);
    try testing.expectEqual(@as(usize, 1), ram_pub.proofs_seen);
    try testing.expectEqual(pub0 + 1, record_ship.stats.foreign_publish_ships.load());
    try testing.expectEqual(pubs0 + 1, record_ship.stats.foreign_publish_served.load());
    // A LOCAL file ships nothing.
    const mine = gino(51, 1);
    try f.a.seedFile(mine, 1);
    try testing.expectEqual(@as(?record_ship.Reply, null), try record_ship.ship(f.a.plane.resolver, 0, mine, .{ .setattr = .{ .mode = 0o600 } }, 0));
    try testing.expectEqual(record_ship.stats.record_ships.load() - ships0, record_ship.stats.record_served.load() - served0);
    // A slot that moved: B's lease view says "not mine" ⇒ slot_moved ⇒ A
    // re-resolves once; a stale view twice is the retryable class.
    _ = f.b.store.leased.orderedRemove(0);
    f.a.assume_other_holds = 3;
    const redir0 = record_ship.stats.record_ship_redirects.load();
    try testing.expectError(error.SlotMoved, record_ship.ship(f.a.plane.resolver, 0, file, .{ .setattr = .{ .mode = 0o600 } }, 0));
    try testing.expectEqual(redir0 + 1, record_ship.stats.record_ship_redirects.load());
    try testing.expectEqual(@as(u32, 1), f.refreshes);
    f.a.assume_other_holds = null;
    try f.b.store.leased.insert(gpa, 0, 3);
}

// ---------------------------------------------------------------------------
// Directory striping over the two holders.
// ---------------------------------------------------------------------------

const Striping = stripe.Striping(HolderStore);

/// The directory source of holder `h`: a directory's pages at ITS holder
/// (the divert stand-in), the record at its holder.
const StripeSource = struct {
    h: *Holder,
    fn holderOf(self: *StripeSource, dir: u64) *Holder {
        return if (self.h.store.leases(route_table.slotOf(dir))) self.h else self.h.other();
    }
    fn page(ctx: *anyopaque, arena: Allocator, dir: u64, after: u64, max: usize, out: *std.ArrayList(stripe.DirRec)) errors.Error!bool {
        const self: *StripeSource = @ptrCast(@alignCast(ctx));
        return stripe.pageFromStore(HolderStore, &self.holderOf(dir).store, arena, dir, after, max, out);
    }
    fn inode(ctx: *anyopaque, ino: u64) errors.Error!?InodeValue {
        const self: *StripeSource = @ptrCast(@alignCast(ctx));
        return self.holderOf(ino).plane.apply.readInode(ino) catch error.InputOutput;
    }
    fn src(self: *StripeSource) stripe.DirSource {
        return .{ .ctx = self, .page = &page, .inode = &inode };
    }
    fn mintOwn(ctx: *anyopaque, _: u64) ?u64 {
        const self: *StripeSource = @ptrCast(@alignCast(ctx));
        return self.h.mint();
    }
    fn minter(self: *StripeSource) stripe.Minter {
        return .{ .ctx = self, .mint = &mintOwn };
    }
    /// The striping face of holder `h` (the `SupplyStripeInos` served
    /// side): a batch of stripe records minted in ITS rotor, one entry.
    fn isDomain(_: *anyopaque, _: u64) bool {
        return false;
    }
    fn supply(ctx: *anyopaque, _: u64, attrs: ops.StripeAttrs, indices: []const u8, out: []u64) usize {
        const self: *StripeSource = @ptrCast(@alignCast(ctx));
        for (indices, 0..) |_, i| out[i] = self.h.mint();
        stripe.writeStripeRecords(HolderStore, &self.h.store, out[0..indices.len], attrs) catch return 0;
        return indices.len;
    }
    fn flipFace(_: *anyopaque, _: u64, _: u8) errors.Error!void {
        return error.NotSupported;
    }
    fn face(self: *StripeSource) ops.StripingFace {
        return .{ .ctx = self, .isDomain = &isDomain, .supply = &supply, .flip = &flipFace };
    }
};

fn namesOf(gpa: Allocator, recs: []const stripe.DirRec) ![][]const u8 {
    const out = try gpa.alloc([]const u8, recs.len);
    for (recs, 0..) |r, i| out[i] = r.name;
    return out;
}

test "a served mutation's reply follows the holder's durability lane (found by the shared hot directory row: fsck C9 found a live inode named by no dentry after kill -9 of the stripes' holder — the holder had answered a served insert before its window landed, the creator retired its intent on that answer, and the dentry died with the holder's ring): a served step, a served record verb and a served stripe supply each leave the holder durable through its last commit before the reply" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    try f.b.seedDir(DIR_B);
    // The served step: A's create in B's directory.
    const c0 = f.b.store.commits;
    _ = try f.a.create(DIR_B, "f", S.IFREG | 0o644);
    try testing.expect(f.b.store.commits > c0);
    try testing.expectEqual(f.b.store.commits, f.b.store.durable);
    // The served record verb: a chmod of B's file from A.
    var view: HolderView = .{};
    var rs_b: RecordShipService = .{ .plane = &f.b.plane, .sink = view.sink() };
    rs_b.register(&f.b.svc);
    const file = gino(50, 3);
    try f.b.seedFile(file, 1);
    const c1 = f.b.store.commits;
    _ = (try record_ship.ship(f.a.plane.resolver, 0, file, .{ .setattr = .{ .mode = 0o600 } }, 0)).?;
    try testing.expect(f.b.store.commits > c1);
    try testing.expectEqual(f.b.store.commits, f.b.store.durable);
    // The served supply: B asks A for two stripe inos of B's directory.
    var src_a: StripeSource = .{ .h = &f.a };
    f.a.plane.striping = src_a.face();
    var supply_a: stripe.SupplyService(HolderStore) = .{ .plane = &f.a.plane };
    supply_a.register(&f.a.svc);
    const c2 = f.a.store.commits;
    var minted: [2]u64 = undefined;
    try testing.expectEqual(@as(usize, 2), try stripe.supplyRemote(linkOf(&f.b.conn), 0, DIR_B, .{ .mode = S.IFDIR | 0o755, .uid = 1, .gid = 2, .ts_ns = 1_000 }, &.{ 0, 1 }, &minted));
    try testing.expect(f.a.store.commits > c2);
    try testing.expectEqual(f.a.store.commits, f.a.store.durable);
}

test "striping: a flip supplied by the other creator OVER THE WIRE (`SupplyStripeInos`, one batch for its share) maps K stripes under ONE intent (the commit marker last), every stripe record flagged; a create into the striped directory keys the STRIPE (shipped when the stripe is foreign); a name left in D re-homes lazily on its next mutation and reads from the stripe (LWW); readdir is the K-way merge in cookie order with a stable continuation across the flip; stat folds nlink and times; rmdir is ONE intent and leaves no map, no stripe, no name; C17 is clean" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    const K: u8 = 4;
    var src_b: StripeSource = .{ .h = &f.b };
    var st = try Striping.init(&f.b.plane, src_b.src(), src_b.minter(), K, .{ .fixed = 2 });
    defer st.deinit();
    // A serves the supply on its wire face (the creator's side).
    var src_a: StripeSource = .{ .h = &f.a };
    f.a.plane.striping = src_a.face();
    var supply_a: stripe.SupplyService(HolderStore) = .{ .plane = &f.a.plane };
    supply_a.register(&f.a.svc);
    var arena_state = std.heap.ArenaAllocator.init(gpa);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    // B's directory with four names before the flip (A created two of them).
    try f.b.seedDir(DIR_B);
    const n1 = try f.b.create(DIR_B, "one", S.IFREG | 0o644);
    _ = try f.b.create(DIR_B, "two", S.IFREG | 0o644);
    _ = try f.a.create(DIR_B, "three", S.IFREG | 0o644);
    _ = try f.a.create(DIR_B, "four", S.IFREG | 0o644);
    var before: std.ArrayList(stripe.DirRec) = .empty;
    _ = try stripe.pageFromStore(HolderStore, &f.b.store, arena, DIR_B, 0, 64, &before);
    try testing.expectEqual(@as(usize, 4), before.items.len);
    // The flip: A supplies its share (K / 2 = 2 stripes in ONE ask over
    // the wire), B mints the rest.
    const flips0 = stripe.stats.dir_stripe_flips.load();
    const sup0 = stripe.stats.dir_stripe_supply_rpcs.load();
    const supplied0 = stripe.stats.dir_stripe_supplied.load();
    const served0 = stripe.stats.dir_stripe_supplies_served.load();
    var map = try st.flip(DIR_B, K, &.{1});
    try testing.expectEqual(flips0 + 1, stripe.stats.dir_stripe_flips.load());
    try testing.expectEqual(sup0 + 1, stripe.stats.dir_stripe_supply_rpcs.load());
    try testing.expectEqual(supplied0 + 2, stripe.stats.dir_stripe_supplied.load());
    try testing.expectEqual(served0 + 1, stripe.stats.dir_stripe_supplies_served.load());
    try testing.expectEqual(K, map.k);
    try testing.expectEqual(@as(u16, 1), route_table.slotOf(map.stripes[0]));
    try testing.expectEqual(@as(u16, 1), route_table.slotOf(map.stripes[1]));
    try testing.expectEqual(@as(u16, 3), route_table.slotOf(map.stripes[2]));
    try testing.expectEqual(@as(u16, 3), route_table.slotOf(map.stripes[3]));
    // Every stripe record carries the flag at its holder; D does not.
    for (map.slice()) |s| {
        const hh: *Holder = if (f.a.store.leases(route_table.slotOf(s))) &f.a else &f.b;
        try testing.expect(stripe.isStripeRecord((try hh.inode(s)).?));
    }
    try testing.expect(!stripe.isStripeRecord((try f.b.inode(DIR_B)).?));
    const read_back = (try stripe.readMap(src_b.src(), arena, DIR_B)).?;
    try testing.expectEqualSlices(u64, map.slice(), read_back.slice());
    try testing.expect(read_back.migrating);
    // A stripe never carries a map, never flips; a second flip refuses.
    try testing.expect((try stripe.readMap(src_b.src(), arena, map.stripes[2])) == null);
    try testing.expectError(error.IsAStripe, st.flip(map.stripes[2], K, &.{}));
    try testing.expectError(error.AlreadyStriped, st.flip(DIR_B, K, &.{}));
    try testing.expectError(error.StripingOff, st.flip(DIR_A, 1, &.{}));
    // readdir across the flip: the same four names, markers never shown,
    // cookie order, a stable continuation.
    var merged: std.ArrayList(stripe.DirRec) = .empty;
    try stripe.readdirMerged(src_b.src(), arena, &map, 0, 64, &merged);
    try testing.expectEqual(@as(usize, 4), merged.items.len);
    for (merged.items, 0..) |r, i| {
        try testing.expect(!stripe.isMarkerName(r.name));
        try testing.expectEqual(before.items[i].cookie, r.cookie);
    }
    // A create into the striped directory keys the stripe of hash(name).
    const seed = f.b.store.nameKey();
    const route_new = map.routeName("five", seed);
    const key_parent = try st.mutationParent(&map, "five", 2_000);
    try testing.expectEqual(route_new.stripe, key_parent);
    const ships0 = stripe.stats.dir_stripe_ships.load();
    const five = try f.a.create(key_parent, "five", S.IFREG | 0o644);
    if (!f.a.store.leases(route_table.slotOf(key_parent))) stripe.stats.dir_stripe_ships.inc();
    try testing.expect(stripe.stats.dir_stripe_ships.load() >= ships0);
    try testing.expectEqual(five, (try stripe.locate(src_b.src(), arena, &map, "five", seed)).?.child);
    // A name still in D's tree reads from D (the secondary home) and
    // re-homes on its next mutation: one migrated name, LWW the stripe.
    try testing.expectEqual(n1, (try stripe.locate(src_b.src(), arena, &map, "one", seed)).?.child);
    const rehomed0 = stripe.stats.dir_stripe_rehomed_on_touch.load();
    const one_parent = try st.mutationParent(&map, "one", 2_000);
    try testing.expectEqual(rehomed0 + 1, stripe.stats.dir_stripe_rehomed_on_touch.load());
    try testing.expectEqual(map.routeName("one", seed).stripe, one_parent);
    try testing.expect((try f.b.dentry(DIR_B, "one")) == null);
    try testing.expectEqual(n1, (try stripe.locate(src_b.src(), arena, &map, "one", seed)).?.child);
    // The second touch re-homes nothing.
    _ = try st.mutationParent(&map, "one", 2_000);
    try testing.expectEqual(rehomed0 + 1, stripe.stats.dir_stripe_rehomed_on_touch.load());
    // The merge still lists five names once each, in cookie order, and a
    // continuation from the second cookie resumes strictly after it.
    merged.clearRetainingCapacity();
    try stripe.readdirMerged(src_b.src(), arena, &map, 0, 64, &merged);
    try testing.expectEqual(@as(usize, 5), merged.items.len);
    for (merged.items[1..], 1..) |r, i| try testing.expect(r.cookie > merged.items[i - 1].cookie);
    var rest: std.ArrayList(stripe.DirRec) = .empty;
    try stripe.readdirMerged(src_b.src(), arena, &map, merged.items[1].cookie, 64, &rest);
    try testing.expectEqual(@as(usize, 3), rest.items.len);
    try testing.expectEqual(merged.items[2].cookie, rest.items[0].cookie);
    // The background migration moves the rest; `migrating` clears.
    const moved = try st.migrate(&map, 2_000);
    try testing.expect(moved >= 3);
    try testing.expect(!map.migrating);
    try testing.expect(!(try stripe.readMap(src_b.src(), arena, DIR_B)).?.migrating);
    try testing.expect(!(try stripe.hasUserName(src_b.src(), arena, DIR_B)));
    // stat D folds the stripes' counts and times (a subdirectory in a
    // stripe adds to nlink; the stripe's mtime wins).
    _ = try f.b.create(try st.mutationParent(&map, "sub", 3_000), "sub", S.IFDIR | 0o755);
    const d = (try f.b.inode(DIR_B)).?;
    const folded = try st.statFold(&map, d, 10_000 * std.time.ns_per_ms);
    try testing.expectEqual(d.nlink + 1, folded.nlink);
    try testing.expect(folded.mtime >= d.mtime);
    const batches0 = stripe.stats.dir_stripe_time_batches.load();
    _ = try st.statFold(&map, d, 20_000 * std.time.ns_per_ms);
    try testing.expect(stripe.stats.dir_stripe_time_batches.load() >= batches0);
    // C17 over the whole tree: clean.
    {
        var views: std.ArrayList(stripe.DirView) = .empty;
        var recs: std.ArrayList(stripe.DirRec) = .empty;
        _ = try stripe.pageFromStore(HolderStore, &f.b.store, arena, DIR_B, 0, 512, &recs);
        var markers: std.ArrayList(stripe.Marker) = .empty;
        var names: std.ArrayList(stripe.DirRec) = .empty;
        for (recs.items) |r| if (stripe.isMarkerName(r.name)) try markers.append(arena, .{ .name = r.name, .child = r.child }) else try names.append(arena, r);
        try views.append(arena, .{ .dir = DIR_B, .markers = markers.items, .names = names.items });
        for (map.slice()) |s| {
            var sr: std.ArrayList(stripe.DirRec) = .empty;
            _ = try src_b.src().page(&src_b, arena, s, 0, 512, &sr);
            try views.append(arena, .{ .dir = s, .markers = &.{}, .names = sr.items });
        }
        const Has = struct {
            var holders: ?*TwoHolders = null;
            fn has(ino: u64) bool {
                const h = holders.?;
                const hh: *Holder = if (h.a.store.leases(route_table.slotOf(ino))) &h.a else &h.b;
                return (hh.plane.apply.readInode(ino) catch null) != null;
            }
        };
        Has.holders = f;
        var findings: std.ArrayList(stripe.Finding) = .empty;
        defer findings.deinit(gpa);
        try stripe.verifyStripes(gpa, views.items, &Has.has, seed, &findings);
        try testing.expectEqual(@as(usize, 0), findings.items.len);
    }
    // rmdir refuses while names remain; after they go it is ONE intent
    // under ONE set over D and every stripe, and nothing is left.
    const parent_of_b = gino(5, 4);
    try f.b.seedDir(parent_of_b);
    _ = try f.b.plane.apply.apply(.{ .insert_dentry = .{ .parent = parent_of_b, .name = "dirb", .child = DIR_B, .ft_bits = S.IFDIR, .parent_update = .exclusive_times_bump } }, .none, .none, 1);
    try testing.expectEqual(@as(u32, 3), (try f.b.inode(parent_of_b)).?.nlink);
    try testing.expectError(error.NotEmpty, st.rmdir(parent_of_b, "dirb", 4_000));
    for ([_][]const u8{ "one", "two", "three", "four", "five" }) |nm| {
        const kp = try st.mutationParent(&map, nm, 4_000);
        try f.b.unlink(kp, nm, false);
    }
    try f.b.unlink(try st.mutationParent(&map, "sub", 4_000), "sub", true);
    const s0 = Snapshot.take();
    const rmdirs0 = stripe.stats.dir_stripe_rmdirs.load();
    try st.rmdir(parent_of_b, "dirb", 5_000);
    try testing.expectEqual(rmdirs0 + 1, stripe.stats.dir_stripe_rmdirs.load());
    try testing.expectEqual(s0.minted + 1, intent.stats.xv_cross_owner_intents_minted.load());
    try testing.expect((try f.b.dentry(parent_of_b, "dirb")) == null);
    try testing.expectEqual(@as(u32, 2), (try f.b.inode(parent_of_b)).?.nlink);
    try testing.expectEqual(@as(u32, 0), (try f.b.inode(DIR_B)).?.nlink);
    try testing.expect((try stripe.readMap(src_b.src(), arena, DIR_B)) == null);
    for (map.slice()) |s| {
        const hh: *Holder = if (f.a.store.leases(route_table.slotOf(s))) &f.a else &f.b;
        const v = try hh.inode(s);
        try testing.expect(v == null or v.?.nlink == 0);
    }
    try f.noOpenIntents();
}

test "the automatic trigger: foreign ships into one directory from MORE THAN ONE creator past N_floor flip it (the census law); one creator alone never flips; the knob at 1 flips nothing" {
    const gpa = testing.allocator;
    const f = try TwoHolders.init(gpa, testing.io, .{}, .{});
    defer f.deinit();
    var src_b: StripeSource = .{ .h = &f.b };
    var st = try Striping.init(&f.b.plane, src_b.src(), src_b.minter(), 2, .{ .fixed = 3 });
    defer st.deinit();
    f.b.plane.served_insert = st.servedInsertSink();
    try f.b.seedDir(DIR_B);
    var arena_state = std.heap.ArenaAllocator.init(gpa);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const flips0 = stripe.stats.dir_stripe_flips.load();
    // One creator (A, the shipped child's slot holder — no requester word
    // needed) ships four names: a handover candidate, never a stripe.
    for ([_][]const u8{ "a1", "a2", "a3", "a4" }) |nm| _ = try f.a.create(DIR_B, nm, S.IFREG | 0o644);
    try testing.expectEqual(flips0, stripe.stats.dir_stripe_flips.load());
    try testing.expectEqual(@as(u32, 1), st.census.dirs.get(DIR_B).?.distinct());
    try testing.expect((try stripe.readMap(src_b.src(), arena, DIR_B)) == null);
    // A second creator's served ship crosses the floor: the flip is DUE
    // and runs on the cadence, never inside the served step (whose
    // initiator still holds its scope over `I{D}` here).
    f.b.plane.served_insert.?.note(f.b.plane.served_insert.?.ctx, DIR_B, 9);
    try testing.expectEqual(flips0, stripe.stats.dir_stripe_flips.load());
    try testing.expectEqual(@as(usize, 1), st.pending.items.len);
    st.runPending();
    try testing.expectEqual(flips0 + 1, stripe.stats.dir_stripe_flips.load());
    const map = (try stripe.readMap(src_b.src(), arena, DIR_B)).?;
    // A ship INTO a stripe feeds no census (the face says so).
    f.b.plane.striping = src_b.face();
    const StripeDomain = struct {
        var m: ?stripe.StripeMap = null;
        fn isDomain(_: *anyopaque, ino: u64) bool {
            return m.?.dir == ino or m.?.isStripe(ino);
        }
    };
    StripeDomain.m = map;
    f.b.plane.striping.?.isDomain = &StripeDomain.isDomain;
    const dirs0 = st.census.dirs.count();
    _ = try f.a.create(map.stripes[0], "into-stripe", S.IFREG | 0o644);
    try testing.expectEqual(dirs0, st.census.dirs.count());
    // The control arm: K = 1 flips nothing.
    var off = try Striping.init(&f.b.plane, src_b.src(), src_b.minter(), 1, .{ .fixed = 1 });
    defer off.deinit();
    try f.b.seedDir(gino(3, 3));
    try testing.expectError(error.StripingOff, off.flip(gino(3, 3), 1, &.{}));
}

test "the stats family names every instrument of the xv namespaces" {
    var buf: [65536]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try @import("../xv.zig").writeStatsJson(&w);
    const s = w.buffered();
    for ([_][]const u8{ "\"intents\":{", "\"xv_cross_owner_intents_stuck\":", "\"applier\":{", "\"xv_steps_already_applied\":", "\"guards\":{", "\"xv_cross_owner_guard_rpcs\":", "\"dir_rename\":{", "\"dir_rename_lock_acquires\":", "\"ops\":{", "\"xv_cross_owner_steps_shipped\":", "\"xv_cross_owner_phase_total\":" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}
