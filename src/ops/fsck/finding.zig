//! ONE tagged finding union over C1–C17 (decision 0003 §G; 0002 Q2 of
//! `ops_fsck.md` §11): the tag IS the class and the payload IS the byte-
//! exact identity — no string class, no string object, no optional
//! identity an older binary could omit. The evidence line is RENDERED from
//! the payload by `format`, never parsed. Beside it: the report (the
//! durable JSON, the bounded admin view, the text form, the exit verdict),
//! the run counters and `merge` for `fsck merge-reports` (§5: findings
//! sort + dedup by identity, census counters SUM, per-run gauges MAX).
//! Portable; pure over bytes.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const key = @import("../../meta/key.zig");
const stripe = @import("../../meta/sym/xv/stripe.zig");
const fsck = @import("../fsck.zig");

pub const Class = enum(u8) { c1 = 1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15, c16, c17 };

/// `loss` is the operator's stop-and-read signal (charter §8 "the LOSS
/// direction"); `warn` a repairable or report-only inconsistency; `info`
/// a leak the next release/reclaim closes.
pub const Severity = enum { info, warn, loss };

/// The repair verbs (`ops_fsck.md` §8); `report_only` refuses with its reason.
pub const Verb = enum {
    quarantine_report_only,
    rebuild_in_place,
    report_only,
    free_leaked_block,
    quarantine_mapping,
    verify_content_then_repair_allocator_or_quarantine,
    recount_and_set_refcount,
    quarantine_then_discard_custody,
    quarantine_staging_dir,
    recompute_accounting,
    release_stale_block_refs,
    destroy_unreferenced_inode,
    lower_nlink_to_counted_names,
    raise_nlink_to_counted_names,
    remove_dangling_dentry,
    return_orphan_image_extent,
    recover_dead_appender,
};

/// C5's verdict classes (the v4 marker + `staging.bindVerdict`).
pub const StagingVerdict = enum { dead, foreign_scope, unreadable };
/// C7's verified arm that failed.
pub const ScrubArm = enum { aead, frame, read, short_read };
/// C1's raw-key defect class (`key.RawKeyDefect`'s tag).
pub const RawKeyDefect = enum { malformed_known_kind, unknown_kind, truncated, slot_out_of_namespace };

/// C10's two count arms share one payload (the direction is the tag).
pub const NlinkCount = struct { ino: u64, nlink: u32, names: u32 };

pub const Finding = union(enum) {
    /// An unreadable node: the walk stops at `cursor` (quarantine-report-only).
    c1_torn: struct { vol: u16, slot: u32, cursor: key.KeyBuf },
    /// A record that fails its kind's schema (or breaks key order).
    c1_semantic: struct { vol: u16, slot: u32, key: key.KeyBuf },
    /// A slot-tree key the forest codec refuses.
    c1_raw_key: struct { vol: u16, slot: u32, key: key.KeyBuf, defect: RawKeyDefect },
    c2_leaked: struct { vol_tag: u64, block_idx: u64 },
    c2_lost: struct { vol_tag: u64, block_idx: u64, ino: u64, block_index: u32, unrepairable_shape: bool },
    c3_refcount: struct { vol_tag: u64, block_idx: u64, expected: u32, actual: u32 },
    c4_orphan: struct { file_id: [16]u8, ino: u64 },
    c5_staging: struct { verdict: StagingVerdict },
    c6_drift: struct { vol_tag: u64, loss: u64, leak: u64 },
    c7_scrub: struct { ino: u64, block_index: u32, vol_tag: u64, block_idx: u64, arm: ScrubArm },
    c8_drift: struct { vol_tag: u64, block_idx: u64, durable: u32, derived: u32 },
    c9_unreferenced: struct { ino: u64, blocks: u32 },
    c10_nlink_too_high: NlinkCount,
    c10_nlink_too_low: NlinkCount,
    c10_zero_nlink_named: struct { ino: u64, names: u32 },
    /// The dentry RECORD is the identity (a collision chain holds several
    /// keys for one name): `(parent key ino, key suffix)` + the child named.
    c10_dangling: struct { parent: u64, suffix: u64, child: u64 },
    c11_orphan_map_records: struct { ino: u64, records: u64 },
    c11_empty_kvmap_head: struct { ino: u64 },
    c11_run_foreign_shadow: struct { ino: u64, shadows: u64 },
    c12_overlap: struct { vol_tag: u64, block_idx: u64, ino_a: u64, block_index_a: u32, ino_b: u64, block_index_b: u32 },
    c12_overrun: struct { vol_tag: u64, block_idx: u64, ino: u64, block_index: u32 },
    c13_orphan_image: struct { vol: u16, appender: u32, extent: u64 },
    c14_slot_custody_conflict: struct { vol: u16, slot: u16, appender_a: u32, appender_b: u32 },
    c15_unrecovered_appender: struct { vol: u16, appender: u32, node_token: u64, mount_slot: u32, window_entries: u64 },
    c16_shared_index_drift: struct { vol_tag: u64, flag_without_entry: u64, entry_without_flag: u64 },
    c17_stripe: struct { shape: stripe.C17Shape, dir: u64, ino: u64 },

    pub const Tag = std.meta.Tag(Finding);

    pub fn class(self: Finding) Class {
        return switch (self) {
            .c1_torn, .c1_semantic, .c1_raw_key => .c1,
            .c2_leaked, .c2_lost => .c2,
            .c3_refcount => .c3,
            .c4_orphan => .c4,
            .c5_staging => .c5,
            .c6_drift => .c6,
            .c7_scrub => .c7,
            .c8_drift => .c8,
            .c9_unreferenced => .c9,
            .c10_nlink_too_high, .c10_nlink_too_low, .c10_zero_nlink_named, .c10_dangling => .c10,
            .c11_orphan_map_records, .c11_empty_kvmap_head, .c11_run_foreign_shadow => .c11,
            .c12_overlap, .c12_overrun => .c12,
            .c13_orphan_image => .c13,
            .c14_slot_custody_conflict => .c14,
            .c15_unrecovered_appender => .c15,
            .c16_shared_index_drift => .c16,
            .c17_stripe => .c17,
        };
    }

    pub fn severity(self: Finding) Severity {
        return switch (self) {
            .c1_torn, .c2_lost, .c7_scrub, .c8_drift, .c10_nlink_too_low, .c10_zero_nlink_named, .c10_dangling, .c12_overlap => .loss,
            .c6_drift => |d| if (d.loss != 0) .loss else .info,
            .c2_leaked, .c10_nlink_too_high, .c13_orphan_image, .c16_shared_index_drift => .info,
            else => .warn,
        };
    }

    /// The repair verb (`ops_fsck.md` §3 per class).
    pub fn verb(self: Finding) Verb {
        return switch (self) {
            .c1_torn => .quarantine_report_only,
            .c1_semantic => .rebuild_in_place,
            // The deletion gate: a KNOWN kind in a shape it never takes is
            // dropped; an unknown kind or a truncated key is never deleted.
            .c1_raw_key => |r| if (r.defect == .malformed_known_kind or r.defect == .slot_out_of_namespace) .rebuild_in_place else .report_only,
            .c2_leaked => .free_leaked_block,
            .c2_lost => |l| if (l.unrepairable_shape) .quarantine_mapping else .verify_content_then_repair_allocator_or_quarantine,
            .c3_refcount => .recount_and_set_refcount,
            .c4_orphan => .quarantine_then_discard_custody,
            .c5_staging => .quarantine_staging_dir,
            .c6_drift => .recompute_accounting,
            .c7_scrub => .quarantine_mapping,
            .c8_drift => .release_stale_block_refs,
            .c9_unreferenced => .destroy_unreferenced_inode,
            .c10_nlink_too_high => .lower_nlink_to_counted_names,
            .c10_nlink_too_low, .c10_zero_nlink_named => .raise_nlink_to_counted_names,
            .c10_dangling => .remove_dangling_dentry,
            .c11_orphan_map_records, .c11_empty_kvmap_head, .c11_run_foreign_shadow, .c12_overlap, .c12_overrun, .c14_slot_custody_conflict, .c16_shared_index_drift, .c17_stripe => .report_only,
            .c13_orphan_image => .return_orphan_image_extent,
            .c15_unrecovered_appender => .recover_dead_appender,
        };
    }

    pub fn reportOnly(self: Finding) bool {
        return self.verb() == .report_only;
    }

    /// Identity order: the tag, then every payload field in declaration
    /// order (key buffers by their bytes).
    pub fn order(a: Finding, b: Finding) std.math.Order {
        const ta = @backingInt(std.meta.activeTag(a));
        const tb = @backingInt(std.meta.activeTag(b));
        if (ta != tb) return std.math.order(ta, tb);
        switch (a) {
            inline else => |pa, tag| {
                const pb = @field(b, @tagName(tag));
                const info = @typeInfo(@TypeOf(pa)).@"struct";
                inline for (info.field_names, info.field_types) |name, T| {
                    const o = orderField(T, @field(pa, name), @field(pb, name));
                    if (o != .eq) return o;
                }
                return .eq;
            },
        }
    }

    pub fn eql(a: Finding, b: Finding) bool {
        return order(a, b) == .eq;
    }

    pub fn lessThan(_: void, a: Finding, b: Finding) bool {
        return order(a, b) == .lt;
    }

    /// The evidence row: `[Cn] object — evidence`.
    pub fn format(self: Finding, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("[C{d}] ", .{@backingInt(self.class())});
        switch (self) {
            .c1_torn => |f| try w.print("meta vol {d} slot {d} at {x} — torn: an unreadable node stopped the walk (no replicas; report-only, data loss made visible)", .{ f.vol, f.slot, f.cursor.slice() }),
            .c1_semantic => |f| try w.print("meta vol {d} slot {d} key {x} — a record that fails its kind's schema or breaks key order", .{ f.vol, f.slot, f.key.slice() }),
            .c1_raw_key => |f| try w.print("meta vol {d} slot {d} key {x} — the forest codec refuses the key ({t})", .{ f.vol, f.slot, f.key.slice(), f.defect }),
            .c2_leaked => |f| try w.print("vol-{x:0>16} block {d} — leaked: tracked by the allocator, referenced by nothing", .{ f.vol_tag, f.block_idx }),
            .c2_lost => |f| try w.print("vol-{x:0>16} block {d} ← ino {d} index {d} — lost: referenced but {s}", .{ f.vol_tag, f.block_idx, f.ino, f.block_index, if (f.unrepairable_shape) "out of range / unaligned / unknown volume" else "untracked by the allocator" }),
            .c3_refcount => |f| try w.print("vol-{x:0>16} block {d} — refcount {d} vs {d} reference(s)", .{ f.vol_tag, f.block_idx, f.actual, f.expected }),
            .c4_orphan => |f| try w.print("staging custody {x} ino {d} — orphan: the ino has no live record", .{ &f.file_id, f.ino }),
            .c5_staging => |f| try w.print("staging dir — marker {t}: not rebindable to this set", .{f.verdict}),
            .c6_drift => |f| try w.print("vol-{x:0>16} — capacity drift: {d} referenced-but-clear (LOSS), {d} set-but-unreferenced (leak)", .{ f.vol_tag, f.loss, f.leak }),
            .c7_scrub => |f| try w.print("ino {d} index {d} vol-{x:0>16} block {d} — scrub {t} failed", .{ f.ino, f.block_index, f.vol_tag, f.block_idx, f.arm }),
            .c8_drift => |f| try w.print("vol-{x:0>16} block {d} — durable refs {d} vs derived {d}", .{ f.vol_tag, f.block_idx, f.durable, f.derived }),
            .c9_unreferenced => |f| try w.print("ino {d} — live (nlink > 0), prior-era, named by no dentry; {d} block(s)", .{ f.ino, f.blocks }),
            .c10_nlink_too_high => |f| try w.print("ino {d} — nlink {d} above {d} name(s) (the leak direction)", .{ f.ino, f.nlink, f.names }),
            .c10_nlink_too_low => |f| try w.print("ino {d} — nlink {d} BELOW {d} name(s) (the LOSS direction: stop and read)", .{ f.ino, f.nlink, f.names }),
            .c10_zero_nlink_named => |f| try w.print("ino {d} — nlink 0 while {d} path(s) resolve (the LOSS direction: stop and read)", .{ f.ino, f.names }),
            .c10_dangling => |f| try w.print("dentry parent {d} suffix {x} → child {d} — dangling: the child has no record", .{ f.parent, f.suffix, f.child }),
            .c11_orphan_map_records => |f| try w.print("ino {d} — {d} block-map record(s) with no live kvmap head (report-only)", .{ f.ino, f.records }),
            .c11_empty_kvmap_head => |f| try w.print("ino {d} — a sweep-less kvmap head with a size and ZERO records (report-only)", .{f.ino}),
            .c11_run_foreign_shadow => |f| try w.print("ino {d} — {d} foreign-volume point(s) inside a covering run (report-only)", .{ f.ino, f.shadows }),
            .c12_overlap => |f| try w.print("vol-{x:0>16} block {d} — tenants ino {d}:{d} and ino {d}:{d} intersect at different offsets (report-only)", .{ f.vol_tag, f.block_idx, f.ino_a, f.block_index_a, f.ino_b, f.block_index_b }),
            .c12_overrun => |f| try w.print("vol-{x:0>16} block {d} ← ino {d} index {d} — a tenant window the window law refuses (report-only)", .{ f.vol_tag, f.block_idx, f.ino, f.block_index }),
            .c13_orphan_image => |f| try w.print("meta vol {d} appender {d} extent {d} — an image a grant claims that no root reaches", .{ f.vol, f.appender, f.extent }),
            .c14_slot_custody_conflict => |f| try w.print("meta vol {d} slot {d} — Live on two pages: appenders {d} and {d}; the mount refuses; remedy `squeezefs appender clear`", .{ f.vol, f.slot, f.appender_a, f.appender_b }),
            .c15_unrecovered_appender => |f| try w.print("meta vol {d} appender {d} ({x}/{d}) — dead, {d} window entr(ies) un-recovered; the mount path recovers it before serving", .{ f.vol, f.appender, f.node_token, f.mount_slot, f.window_entries }),
            .c16_shared_index_drift => |f| try w.print("vol-{x:0>16} — shared index: {d} flag(s) without entry, {d} entr(ies) without flag (report-only)", .{ f.vol_tag, f.flag_without_entry, f.entry_without_flag }),
            .c17_stripe => |f| try w.print("dir {d} ino {d} — stripe map {t} (report-only)", .{ f.dir, f.ino, f.shape }),
        }
    }

    /// `{"kind":"<tag>", <field>:<value>, …}` — snake_case, a key buffer /
    /// byte array as hex, an enum as its tag name.
    pub fn writeJson(self: Finding, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            inline else => |p, tag| {
                try w.print("{{\"kind\":\"{s}\"", .{@tagName(tag)});
                const info = @typeInfo(@TypeOf(p)).@"struct";
                inline for (info.field_names, info.field_types) |name, T| {
                    try w.print(",\"{s}\":", .{name});
                    try writeFieldJson(T, @field(p, name), w);
                }
                try w.writeByte('}');
            },
        }
    }

    pub const ParseError = error{ Corrupt, OutOfMemory };

    /// The inverse of `writeJson` over a parsed object (every field present
    /// and well-typed, the kind a known tag — forward-only: an unknown kind
    /// refuses the whole report).
    pub fn fromJson(obj: std.json.ObjectMap) ParseError!Finding {
        const kind_v = obj.get("kind") orelse return error.Corrupt;
        if (kind_v != .string) return error.Corrupt;
        const tag = std.meta.stringToEnum(Tag, kind_v.string) orelse return error.Corrupt;
        switch (tag) {
            inline else => |t| {
                const P = @FieldType(Finding, @tagName(t));
                var p: P = undefined;
                const info = @typeInfo(P).@"struct";
                inline for (info.field_names, info.field_types) |name, T| {
                    const v = obj.get(name) orelse return error.Corrupt;
                    @field(p, name) = try readFieldJson(T, v);
                }
                return @unionInit(Finding, @tagName(t), p);
            },
        }
    }
};

fn orderField(comptime T: type, a: T, b: T) std.math.Order {
    if (T == key.KeyBuf) return key.order(a.slice(), b.slice());
    return switch (@typeInfo(T)) {
        .int => std.math.order(a, b),
        .bool => std.math.order(@intFromBool(a), @intFromBool(b)),
        .@"enum" => std.math.order(@backingInt(a), @backingInt(b)),
        .array => std.mem.order(u8, &a, &b),
        else => comptime unreachable,
    };
}

fn writeFieldJson(comptime T: type, v: T, w: *std.Io.Writer) std.Io.Writer.Error!void {
    if (T == key.KeyBuf) return w.print("\"{x}\"", .{v.slice()});
    switch (@typeInfo(T)) {
        .int => try w.print("{d}", .{v}),
        .bool => try w.print("{}", .{v}),
        .@"enum" => try w.print("\"{s}\"", .{@tagName(v)}),
        .array => try w.print("\"{x}\"", .{&v}),
        else => comptime unreachable,
    }
}

fn readFieldJson(comptime T: type, v: std.json.Value) Finding.ParseError!T {
    if (T == key.KeyBuf) {
        if (v != .string) return error.Corrupt;
        var buf: key.KeyBuf = .{};
        if (v.string.len % 2 != 0 or v.string.len / 2 > buf.buf.len) return error.Corrupt;
        const n = v.string.len / 2;
        _ = std.fmt.hexToBytes(buf.buf[0..n], v.string) catch return error.Corrupt;
        buf.len = @intCast(n);
        return buf;
    }
    switch (@typeInfo(T)) {
        .int => return jsonInt(T, v),
        .bool => return if (v == .bool) v.bool else error.Corrupt,
        .@"enum" => {
            if (v != .string) return error.Corrupt;
            return std.meta.stringToEnum(T, v.string) orelse error.Corrupt;
        },
        .array => |a| {
            if (v != .string or v.string.len != a.len * 2) return error.Corrupt;
            var out: T = undefined;
            _ = std.fmt.hexToBytes(&out, v.string) catch return error.Corrupt;
            return out;
        },
        else => comptime unreachable,
    }
}

/// A JSON integer as `T`: `std.json` yields `.integer` (i64) or
/// `.number_string` past it (a `vol_tag` above 2⁶³).
fn jsonInt(comptime T: type, v: std.json.Value) Finding.ParseError!T {
    return switch (v) {
        .integer => |i| std.math.cast(T, i) orelse error.Corrupt,
        .number_string => |s| std.fmt.parseInt(T, s, 10) catch error.Corrupt,
        else => error.Corrupt,
    };
}

// ---------------------------------------------------------------------------
// The run counters (§6): plain words a run fills and `publish` folds into
// the global instruments; the merge rules per field class.
// ---------------------------------------------------------------------------

pub const Counters = struct {
    // SUM across shards (each judges its own residue).
    inodes_scanned: u64 = 0,
    dentry_refs_indexed: u64 = 0,
    blocks_checked: u64 = 0,
    refcounts_checked: u64 = 0,
    suspects: u64 = 0,
    suspects_cleared: u64 = 0,
    epoch_exempted: u64 = 0,
    inflight_exempted: u64 = 0,
    pack_ledger_exempted: u64 = 0,
    current_era_exempted: u64 = 0,
    unreferenced_intent_exempted: u64 = 0,
    nlink_transient_cleared: u64 = 0,
    crossing_exempted: u64 = 0,
    alloc_bitmap_tracked_exempted: u64 = 0,
    alloc_census_declined: u64 = 0,
    c1_projection_slots_scoped: u64 = 0,
    c4_custody_scoped: u64 = 0,
    block_plane_foreign_scoped: u64 = 0,
    inode_plane_names_declined: u64 = 0,
    block_plane_member_bindings: u64 = 0,
    inode_plane_foreign_slot_scoped: u64 = 0,
    inode_plane_window_scoped: u64 = 0,
    inode_plane_foreign_dentry_scoped: u64 = 0,
    nlink_names_counted: u64 = 0,
    nlink_mismatch_high: u64 = 0,
    nlink_mismatch_low: u64 = 0,
    nlink_zero_named: u64 = 0,
    dangling_dentries: u64 = 0,
    scrub_blocks_scanned: u64 = 0,
    scrub_bytes_scanned: u64 = 0,
    scrub_aead_verified: u64 = 0,
    scrub_frame_verified: u64 = 0,
    scrub_readability_only: u64 = 0,
    scrub_failures: u64 = 0,
    scrub_pin_unstable_cleared: u64 = 0,
    // The per-suspect lookups' work (0003 §AC.6): elements examined to name
    // a C2-lost block's owner, a C9 candidate's block count, a C10
    // candidate's distinct names, the C17 routing check's stripes, the
    // bindings the ladder's C2-leaked/C3/C6 re-checks derive references
    // from — and the store calls C11's owner scan made. Each is linear in
    // its inputs.
    c2_owner_steps: u64 = 0,
    c9_owner_steps: u64 = 0,
    c10_name_steps: u64 = 0,
    c11_store_calls: u64 = 0,
    c17_route_steps: u64 = 0,
    recheck_derived_steps: u64 = 0,
    // MAX across shards (per-run gauges: every shard walks the whole
    // space). The walk's cost (`census.WalkTally`): tree nodes fetched,
    // entries delivered, entries the trees copied to deliver them, store
    // calls, trees the calls consulted — `ops_fsck.md` §6.1's bound.
    nodes_walked: u64 = 0,
    entries_visited: u64 = 0,
    entries_read: u64 = 0,
    store_calls: u64 = 0,
    trees_consulted: u64 = 0,
    scan_ms: u64 = 0,
    // The wall clock per pass: the census (inode + layout kinds), the
    // dentry pass, C1's raw walk, the other classes' nominations, the
    // ladder's re-check, the scrub (its fresh census included).
    census_ms: u64 = 0,
    dentries_ms: u64 = 0,
    c1_ms: u64 = 0,
    classes_ms: u64 = 0,
    recheck_ms: u64 = 0,
    scrub_ms: u64 = 0,
    inode_plane_slots_covered: u64 = 0,
    // Candidates past the ceiling, said loudly (batching is convergent).
    deferred: u64 = 0,

    const max_fields = [_][]const u8{ "nodes_walked", "entries_visited", "entries_read", "store_calls", "trees_consulted", "scan_ms", "census_ms", "dentries_ms", "c1_ms", "classes_ms", "recheck_ms", "scrub_ms", "inode_plane_slots_covered" };

    fn isMaxField(comptime name: []const u8) bool {
        @setEvalBranchQuota(10_000); // comptime: the field-name match over the max list per counter (the default 1 000 branches ran out at 13 max fields)
        inline for (max_fields) |m| if (comptime std.mem.eql(u8, m, name)) return true;
        return false;
    }

    /// §5: census counters SUM, per-run gauges take the MAX.
    pub fn merge(self: *Counters, other: Counters) void {
        inline for (@typeInfo(Counters).@"struct".field_names) |name| {
            if (comptime isMaxField(name)) {
                @field(self, name) = @max(@field(self, name), @field(other, name));
            } else {
                @field(self, name) +|= @field(other, name);
            }
        }
    }

    pub fn writeJson(self: Counters, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.writeByte('{');
        inline for (@typeInfo(Counters).@"struct".field_names, 0..) |name, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("\"{s}\":{d}", .{ name, @field(self, name) });
        }
        try w.writeByte('}');
    }

    pub fn fromJson(obj: std.json.ObjectMap) Finding.ParseError!Counters {
        var c: Counters = .{};
        inline for (@typeInfo(Counters).@"struct".field_names) |name| {
            if (obj.get(name)) |v| @field(c, name) = try jsonInt(u64, v);
        }
        return c;
    }

    /// Fold into the global `fsck_*` / `scrub_*` instruments (ONE publish
    /// per run — never per suspect, the drift-gauge law).
    pub fn publish(self: Counters) void {
        const s = &fsck.stats;
        s.fsck_inodes_scanned.add(self.inodes_scanned);
        s.fsck_nodes_walked.add(self.nodes_walked);
        s.fsck_entries_visited.add(self.entries_visited);
        s.fsck_entries_read.add(self.entries_read);
        s.fsck_dentry_refs_indexed.add(self.dentry_refs_indexed);
        s.fsck_blocks_checked.add(self.blocks_checked);
        s.fsck_refcounts_checked.add(self.refcounts_checked);
        s.fsck_scan_secs.set(self.scan_ms / 1000);
        s.fsck_inode_plane_slots_covered.set(self.inode_plane_slots_covered);
        s.fsck_suspects.add(self.suspects);
        s.fsck_suspects_cleared.add(self.suspects_cleared);
        s.fsck_epoch_exempted.add(self.epoch_exempted);
        s.fsck_inflight_exempted.add(self.inflight_exempted);
        s.fsck_pack_ledger_exempted.add(self.pack_ledger_exempted);
        s.fsck_current_era_exempted.add(self.current_era_exempted);
        s.fsck_unreferenced_intent_exempted.add(self.unreferenced_intent_exempted);
        s.fsck_nlink_transient_cleared.add(self.nlink_transient_cleared);
        s.fsck_crossing_exempted.add(self.crossing_exempted);
        s.fsck_alloc_bitmap_tracked_exempted.add(self.alloc_bitmap_tracked_exempted);
        s.fsck_alloc_census_declined.add(self.alloc_census_declined);
        s.fsck_c1_projection_slots_scoped.add(self.c1_projection_slots_scoped);
        s.fsck_c4_custody_scoped.add(self.c4_custody_scoped);
        s.fsck_block_plane_foreign_scoped.add(self.block_plane_foreign_scoped);
        s.fsck_inode_plane_names_declined.add(self.inode_plane_names_declined);
        s.fsck_block_plane_member_bindings.add(self.block_plane_member_bindings);
        s.fsck_inode_plane_foreign_slot_scoped.add(self.inode_plane_foreign_slot_scoped);
        s.fsck_inode_plane_window_scoped.add(self.inode_plane_window_scoped);
        s.fsck_inode_plane_foreign_dentry_scoped.add(self.inode_plane_foreign_dentry_scoped);
        s.fsck_nlink_names_counted.add(self.nlink_names_counted);
        s.fsck_nlink_mismatch_high.add(self.nlink_mismatch_high);
        s.fsck_nlink_mismatch_low.add(self.nlink_mismatch_low);
        s.fsck_nlink_zero_named.add(self.nlink_zero_named);
        s.fsck_dangling_dentries.add(self.dangling_dentries);
        s.scrub_blocks_scanned.add(self.scrub_blocks_scanned);
        s.scrub_bytes_scanned.add(self.scrub_bytes_scanned);
        s.scrub_aead_verified.add(self.scrub_aead_verified);
        s.scrub_frame_verified.add(self.scrub_frame_verified);
        s.scrub_readability_only.add(self.scrub_readability_only);
        s.scrub_failures.add(self.scrub_failures);
        s.scrub_pin_unstable_cleared.add(self.scrub_pin_unstable_cleared);
    }
};

// ---------------------------------------------------------------------------
// The repair report (§8): one row per finding with the verb and the outcome.
// ---------------------------------------------------------------------------

pub const Outcome = enum { planned, applied, refused };

pub const RepairRow = struct {
    finding: Finding,
    verb: Verb,
    outcome: Outcome,
    /// The refusal's reason / the plan's detail (a static string).
    detail: []const u8,
};

pub const RepairReport = struct {
    applied_mode: bool,
    rows: []RepairRow,
    quarantined_records: u64 = 0,
    quarantined_blocks: u64 = 0,
    quarantined_bytes: u64 = 0,

    pub fn deinit(self: *RepairReport, gpa: Allocator) void {
        gpa.free(self.rows);
        self.* = undefined;
    }

    pub fn count(self: RepairReport, o: Outcome) u64 {
        var n: u64 = 0;
        for (self.rows) |r| if (r.outcome == o) {
            n += 1;
        };
        return n;
    }

    pub fn writeJson(self: RepairReport, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{{\"apply\":{},\"planned\":{d},\"applied\":{d},\"refused\":{d},\"quarantined_records\":{d},\"quarantined_blocks\":{d},\"quarantined_bytes\":{d},\"actions\":[", .{ self.applied_mode, self.count(.planned), self.count(.applied), self.count(.refused), self.quarantined_records, self.quarantined_blocks, self.quarantined_bytes });
        for (self.rows, 0..) |r, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"verb\":\"{s}\",\"outcome\":\"{s}\",\"detail\":", .{ @tagName(r.verb), @tagName(r.outcome) });
            try writeJsonString(w, r.detail);
            try w.writeAll(",\"finding\":");
            try r.finding.writeJson(w);
            try w.writeByte('}');
        }
        try w.writeAll("]}");
    }
};

// ---------------------------------------------------------------------------
// The report.
// ---------------------------------------------------------------------------

pub const Mode = enum { online, offline };

pub const Shard = struct {
    k: u32,
    n: u32,

    /// `global_ino % n == k`: the ONE partition key (§5).
    pub fn owns(self: Shard, global_ino: u64) bool {
        return global_ino % self.n == self.k;
    }

    /// `"k/N"`, `0 ≤ k < N`, `N ≥ 1`.
    pub fn parse(word: []const u8) ?Shard {
        const slash = std.mem.indexOfScalar(u8, word, '/') orelse return null;
        const k = std.fmt.parseInt(u32, word[0..slash], 10) catch return null;
        const n = std.fmt.parseInt(u32, word[slash + 1 ..], 10) catch return null;
        if (n == 0 or k >= n) return null;
        return .{ .k = k, .n = n };
    }

    pub fn format(self: Shard, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{d}/{d}", .{ self.k, self.n });
    }
};

/// One block a layout binds: the fleet member's derived census row
/// (`block_refs.Binding` on the wire).
pub const BindingRef = struct { vol_tag: u64, block_idx: u64, block_index: u32 };

/// One inode's bindings in a member's derived census (its key ino).
pub const OwnerRef = struct { ino: u64, bindings: []const BindingRef };

pub const Report = struct {
    schema: u32 = fsck.REPORT_SCHEMA,
    mode: Mode,
    shard: ?Shard = null,
    /// Sorted by identity, deduplicated.
    findings: []Finding,
    counters: Counters = .{},
    repair: ?RepairReport = null,
    /// Findings the bounded view dropped (they ARE findings for the verdict).
    findings_elided: u64 = 0,
    /// A fleet member's derived block census over its custody — the inos
    /// of the slots it leases and every block their layouts bind — shipped
    /// in its proposal for the coordinator's merged reference arms (§8:
    /// the holder cannot read a live lessee's layouts); empty on every
    /// published report (`merge` drops it: an input, never an output).
    owners: []const OwnerRef = &.{},

    pub fn deinit(self: *Report, gpa: Allocator) void {
        gpa.free(self.findings);
        if (self.repair) |*r| r.deinit(gpa);
        freeOwners(gpa, self.owners);
        self.* = undefined;
    }

    pub fn freeOwners(gpa: Allocator, owners: []const OwnerRef) void {
        for (owners) |o| gpa.free(o.bindings);
        gpa.free(owners);
    }

    pub fn hasFindings(self: Report) bool {
        return self.findings.len != 0 or self.findings_elided != 0;
    }

    /// Sort + dedup by identity (the normal form every report is in).
    pub fn normalize(findings: []Finding) usize {
        std.mem.sort(Finding, findings, {}, Finding.lessThan);
        if (findings.len == 0) return 0;
        var n: usize = 1;
        for (findings[1..]) |f| {
            if (!f.eql(findings[n - 1])) {
                findings[n] = f;
                n += 1;
            }
        }
        return n;
    }

    pub fn writeJson(self: Report, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try self.writeJsonPrefix(self.findings.len, w);
    }

    fn writeJsonPrefix(self: Report, n: usize, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{{\"schema\":{d},\"mode\":\"{s}\",\"shard\":", .{ self.schema, @tagName(self.mode) });
        if (self.shard) |s| try w.print("\"{f}\"", .{s}) else try w.writeAll("null");
        try w.print(",\"findings_elided\":{d},\"findings\":[", .{self.findings_elided + (self.findings.len - n)});
        for (self.findings[0..n], 0..) |f, i| {
            if (i != 0) try w.writeByte(',');
            try f.writeJson(w);
        }
        try w.writeAll("],\"counters\":");
        try self.counters.writeJson(w);
        try w.writeAll(",\"repair\":");
        if (self.repair) |r| try r.writeJson(w) else try w.writeAll("null");
        if (self.owners.len != 0) {
            // `[[ino,[[vol_tag,block_idx,block_index],…]],…]` — the compact
            // row shape (a custody of 100 k bindings is ~4 MB, inside the
            // proposal cap).
            try w.writeAll(",\"owners\":[");
            for (self.owners, 0..) |o, i| {
                if (i != 0) try w.writeByte(',');
                try w.print("[{d},[", .{o.ino});
                for (o.bindings, 0..) |b, j| {
                    if (j != 0) try w.writeByte(',');
                    try w.print("[{d},{d},{d}]", .{ b.vol_tag, b.block_idx, b.block_index });
                }
                try w.writeAll("]]");
            }
            try w.writeByte(']');
        }
        try w.writeByte('}');
    }

    /// The bounded admin view: the longest finding prefix whose JSON fits
    /// `max_bytes` (a binary search over the prefix length); the dropped
    /// tail is counted in `findings_elided`. Owned by `gpa`.
    pub fn boundedJson(self: Report, gpa: Allocator, max_bytes: usize) Allocator.Error![]u8 {
        var lo: usize = 0;
        var hi: usize = self.findings.len;
        var best: ?[]u8 = null;
        errdefer if (best) |b| gpa.free(b);
        while (lo <= hi) {
            const mid = lo + (hi - lo) / 2;
            var out: std.Io.Writer.Allocating = .init(gpa);
            defer out.deinit();
            self.writeJsonPrefix(mid, &out.writer) catch return error.OutOfMemory;
            if (out.written().len <= max_bytes) {
                if (best) |b| gpa.free(b);
                best = try gpa.dupe(u8, out.written());
                lo = mid + 1;
            } else {
                if (mid == 0) break;
                hi = mid - 1;
            }
        }
        // Even the empty prefix did not fit: the header alone is the view.
        return best orelse try gpa.dupe(u8, "");
    }

    /// The text form (`MAIN:L2405-2475`).
    pub fn writeText(self: Report, w: *std.Io.Writer) std.Io.Writer.Error!void {
        const c = self.counters;
        try w.print("fsck ({s}", .{@tagName(self.mode)});
        if (self.shard) |s| try w.print(", shard {f}", .{s});
        try w.print("): {d} inode(s), {d} dentry ref(s), {d} block(s) checked, {d} refcount(s) checked, {d} node(s) walked in {d}.{d:0>3} s\n", .{ c.inodes_scanned, c.dentry_refs_indexed, c.blocks_checked, c.refcounts_checked, c.nodes_walked, c.scan_ms / 1000, c.scan_ms % 1000 });
        try w.print("walk: {d} entr(ies) visited, {d} read, {d} node(s), {d} store call(s), {d} tree(s) consulted; census {d} ms, dentries {d} ms, C1 {d} ms, classes {d} ms, re-check {d} ms, scrub {d} ms\n", .{ c.entries_visited, c.entries_read, c.nodes_walked, c.store_calls, c.trees_consulted, c.census_ms, c.dentries_ms, c.c1_ms, c.classes_ms, c.recheck_ms, c.scrub_ms });
        try w.print("suspects: {d} ({d} cleared; epoch {d}, in-flight {d}, pack {d}, era {d}, intent {d}, nlink-transient {d}, crossing {d})\n", .{ c.suspects, c.suspects_cleared, c.epoch_exempted, c.inflight_exempted, c.pack_ledger_exempted, c.current_era_exempted, c.unreferenced_intent_exempted, c.nlink_transient_cleared, c.crossing_exempted });
        try w.print("lookups: C2 owners {d}, C9 owners {d}, C10 names {d}, C11 calls {d}, C17 routes {d}, re-check bindings {d}\n", .{ c.c2_owner_steps, c.c9_owner_steps, c.c10_name_steps, c.c11_store_calls, c.c17_route_steps, c.recheck_derived_steps });
        if (c.scrub_blocks_scanned != 0 or c.scrub_failures != 0) {
            try w.print("scrub: {d} block(s), {d} byte(s); aead {d}, frame {d}, readability-only {d}, failures {d}\n", .{ c.scrub_blocks_scanned, c.scrub_bytes_scanned, c.scrub_aead_verified, c.scrub_frame_verified, c.scrub_readability_only, c.scrub_failures });
        }
        if (c.deferred != 0) try w.print("deferred: {d} candidate(s) past the per-run ceiling — repair the reported ones and re-run\n", .{c.deferred});
        if (!self.hasFindings()) {
            try w.writeAll("findings: 0 (clean)\n");
        } else {
            try w.print("findings: {d}\n", .{self.findings.len + self.findings_elided});
            for (self.findings) |f| try w.print("  {f}\n", .{f});
            if (self.findings_elided != 0) try w.print("  … {d} more elided from this view\n", .{self.findings_elided});
        }
        if (self.repair) |r| {
            try w.print("repair ({s}): {d} planned, {d} applied, {d} refused; quarantined {d} record(s), {d} block(s), {d} byte(s)\n", .{ if (r.applied_mode) "--apply" else "dry-run", r.count(.planned), r.count(.applied), r.count(.refused), r.quarantined_records, r.quarantined_blocks, r.quarantined_bytes });
            for (r.rows) |row| try w.print("  {s}: {s} — {s}\n", .{ @tagName(row.outcome), @tagName(row.verb), row.detail });
        }
    }

    /// 0 clean; 1 when findings exist (including elided; also a dry-run
    /// `--repair` with findings) — `cli.exit_failed`'s value.
    pub fn exitCode(self: Report) u8 {
        return if (self.hasFindings()) 1 else 0;
    }

    /// Fold the counters into the instruments and settle the verdict
    /// gauges; a confirmed finding fires THE tripwire once per run.
    pub fn publish(self: Report) void {
        self.counters.publish();
        const n = self.findings.len + self.findings_elided;
        fsck.stats.fsck_findings_last.set(n);
        var conflicts: u64 = 0;
        var unrecovered: u64 = 0;
        for (self.findings) |f| switch (f) {
            .c14_slot_custody_conflict => conflicts += 1,
            .c15_unrecovered_appender => unrecovered += 1,
            .c12_overlap => fsck.stats.fsck_tenant_overlap_findings.fire("two tenants intersect at different offsets"),
            else => {},
        };
        fsck.stats.fsck_slot_custody_conflicts.set(conflicts);
        fsck.stats.fsck_unrecovered_appenders.set(unrecovered);
        if (n != 0) fsck.stats.fsck_findings.fire("fsck confirmed a finding on this volume");
    }

    /// Parse a durable report (merge inputs are durable reports by contract;
    /// a `partial` member is not carried — 0002: the mapping identities blew
    /// the xattr cap). Forward-only: a foreign schema refuses.
    pub fn parse(gpa: Allocator, text: []const u8) Finding.ParseError!Report {
        var parsed = std.json.parseFromSlice(std.json.Value, gpa, text, .{}) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Corrupt,
        };
        defer parsed.deinit();
        if (parsed.value != .object) return error.Corrupt;
        const obj = parsed.value.object;
        const schema = try jsonInt(u32, obj.get("schema") orelse return error.Corrupt);
        if (schema != fsck.REPORT_SCHEMA) return error.Corrupt;
        const mode_v = obj.get("mode") orelse return error.Corrupt;
        if (mode_v != .string) return error.Corrupt;
        const mode = std.meta.stringToEnum(Mode, mode_v.string) orelse return error.Corrupt;
        var shard: ?Shard = null;
        if (obj.get("shard")) |sv| switch (sv) {
            .null => {},
            .string => |s| shard = Shard.parse(s) orelse return error.Corrupt,
            else => return error.Corrupt,
        };
        const fv = obj.get("findings") orelse return error.Corrupt;
        if (fv != .array) return error.Corrupt;
        var findings = try gpa.alloc(Finding, fv.array.items.len);
        errdefer gpa.free(findings);
        for (fv.array.items, 0..) |item, i| {
            if (item != .object) return error.Corrupt;
            findings[i] = try Finding.fromJson(item.object);
        }
        const n = Report.normalize(findings);
        findings = try gpa.realloc(findings, n);
        var counters: Counters = .{};
        if (obj.get("counters")) |cv| {
            if (cv != .object) return error.Corrupt;
            counters = try Counters.fromJson(cv.object);
        }
        const elided = if (obj.get("findings_elided")) |ev| try jsonInt(u64, ev) else 0;
        var owners: []OwnerRef = &.{};
        errdefer freeOwners(gpa, owners);
        if (obj.get("owners")) |ov| {
            if (ov != .array) return error.Corrupt;
            owners = try gpa.alloc(OwnerRef, ov.array.items.len);
            var built: usize = 0;
            errdefer {
                for (owners[0..built]) |o| gpa.free(o.bindings);
                gpa.free(owners);
                owners = &.{};
            }
            for (ov.array.items) |row| {
                if (row != .array or row.array.items.len != 2 or row.array.items[1] != .array) return error.Corrupt;
                const bl = row.array.items[1].array.items;
                const bindings = try gpa.alloc(BindingRef, bl.len);
                errdefer gpa.free(bindings);
                for (bl, 0..) |bv, k| {
                    if (bv != .array or bv.array.items.len != 3) return error.Corrupt; // a binding row is `[vol_tag, block_idx, block_index]`
                    bindings[k] = .{ .vol_tag = try jsonInt(u64, bv.array.items[0]), .block_idx = try jsonInt(u64, bv.array.items[1]), .block_index = try jsonInt(u32, bv.array.items[2]) };
                }
                owners[built] = .{ .ino = try jsonInt(u64, row.array.items[0]), .bindings = bindings };
                built += 1;
            }
        }
        return .{ .mode = mode, .shard = shard, .findings = findings, .counters = counters, .findings_elided = elided, .owners = owners };
    }
};

/// `fsck merge-reports` (§5): findings concatenated then sort + dedup by
/// identity (the per-shard-repeated classes collapse), census counters SUM,
/// per-run gauges MAX, `mode` = the last input's, `shard = null`,
/// `findings_elided = 0` (durable inputs are complete by contract).
pub fn merge(gpa: Allocator, reports: []const Report) Allocator.Error!Report {
    var all: std.ArrayList(Finding) = .empty;
    errdefer all.deinit(gpa);
    var counters: Counters = .{};
    var mode: Mode = .offline;
    for (reports) |r| {
        try all.appendSlice(gpa, r.findings);
        counters.merge(r.counters);
        mode = r.mode;
    }
    const n = Report.normalize(all.items);
    all.items.len = n;
    return .{ .mode = mode, .shard = null, .findings = try all.toOwnedSlice(gpa), .counters = counters };
}

fn writeJsonString(w: *std.Io.Writer, s: []const u8) std.Io.Writer.Error!void {
    try w.writeByte('"');
    for (s) |c| switch (c) {
        '"' => try w.writeAll("\\\""),
        '\\' => try w.writeAll("\\\\"),
        '\n' => try w.writeAll("\\n"),
        else => if (c < 0x20) try w.print("\\u{x:0>4}", .{c}) else try w.writeByte(c),
    };
    try w.writeByte('"');
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn sample() [6]Finding {
    return .{
        .{ .c2_leaked = .{ .vol_tag = 0x10, .block_idx = 7 } },
        .{ .c9_unreferenced = .{ .ino = 4099, .blocks = 2 } },
        .{ .c1_torn = .{ .vol = 0, .slot = 3, .cursor = key.KeyBuf.fromSlice(&key.inodeKey(77)) } },
        .{ .c10_nlink_too_low = .{ .ino = 9, .nlink = 1, .names = 2 } },
        .{ .c4_orphan = .{ .file_id = @splat(0xAB), .ino = 5 } },
        .{ .c17_stripe = .{ .shape = .misrouted_name, .dir = 12, .ino = 13 } },
    };
}

test "the union: every arm has a class, a severity, a verb; report-only arms refuse; identity order is total and dedup collapses equal identities" {
    const tag_info = @typeInfo(Finding.Tag).@"enum";
    inline for (tag_info.field_names, tag_info.field_values) |name, value| {
        const tag: Finding.Tag = @fromBackingInt(@intCast(value));
        const P = @FieldType(Finding, name);
        const fnd = @unionInit(Finding, name, std.mem.zeroes(P));
        _ = fnd.class();
        _ = fnd.severity();
        try testing.expectEqual(fnd.reportOnly(), fnd.verb() == .report_only);
        try testing.expectEqual(tag, std.meta.activeTag(fnd));
    }
    try testing.expect((Finding{ .c11_empty_kvmap_head = .{ .ino = 1 } }).reportOnly());
    try testing.expect(!(Finding{ .c2_leaked = .{ .vol_tag = 1, .block_idx = 1 } }).reportOnly());
    try testing.expectEqual(Severity.loss, (Finding{ .c10_zero_nlink_named = .{ .ino = 1, .names = 1 } }).severity());
    try testing.expectEqual(Severity.info, (Finding{ .c6_drift = .{ .vol_tag = 1, .loss = 0, .leak = 3 } }).severity());
    try testing.expectEqual(Severity.loss, (Finding{ .c6_drift = .{ .vol_tag = 1, .loss = 1, .leak = 0 } }).severity());
    // The raw-key deletion gate.
    try testing.expectEqual(Verb.rebuild_in_place, (Finding{ .c1_raw_key = .{ .vol = 0, .slot = 0, .key = .{}, .defect = .malformed_known_kind } }).verb());
    try testing.expectEqual(Verb.report_only, (Finding{ .c1_raw_key = .{ .vol = 0, .slot = 0, .key = .{}, .defect = .unknown_kind } }).verb());

    var list = sample() ++ sample();
    const n = Report.normalize(&list);
    try testing.expectEqual(@as(usize, 6), n);
    for (list[0 .. n - 1], list[1..n]) |a, b| try testing.expectEqual(std.math.Order.lt, a.order(b));
}

test "report JSON round-trips every arm (hex key buffers, enum tags, u64 fields past i64), the counters and the shard; merge sums, maxes and dedups; the bounded view keeps the verdict" {
    const gpa = testing.allocator;
    var one = sample();
    var big: [1]Finding = .{.{ .c8_drift = .{ .vol_tag = 0xFFFF_FFFF_FFFF_FFF0, .block_idx = 1, .durable = 2, .derived = 1 } }};
    const r1: Report = .{ .mode = .offline, .shard = .{ .k = 0, .n = 2 }, .findings = &one, .counters = .{ .inodes_scanned = 10, .nodes_walked = 4, .scan_ms = 1500 } };
    const r2: Report = .{ .mode = .offline, .shard = .{ .k = 1, .n = 2 }, .findings = &big, .counters = .{ .inodes_scanned = 5, .nodes_walked = 9, .scan_ms = 200 } };
    var out1: std.Io.Writer.Allocating = .init(gpa);
    defer out1.deinit();
    try r1.writeJson(&out1.writer);
    var out2: std.Io.Writer.Allocating = .init(gpa);
    defer out2.deinit();
    try r2.writeJson(&out2.writer);
    var p1 = try Report.parse(gpa, out1.written());
    defer p1.deinit(gpa);
    var p2 = try Report.parse(gpa, out2.written());
    defer p2.deinit(gpa);
    try testing.expectEqual(@as(usize, 6), p1.findings.len);
    try testing.expectEqual(Shard{ .k = 0, .n = 2 }, p1.shard.?);
    try testing.expect(p2.findings[0].eql(big[0]));
    var sorted = sample();
    _ = Report.normalize(&sorted);
    for (p1.findings, sorted) |a, b| try testing.expect(a.eql(b));
    // merge: 7 distinct findings, counters summed, gauges maxed, shard dropped.
    var m = try merge(gpa, &.{ p1, p2, p1 });
    defer m.deinit(gpa);
    try testing.expectEqual(@as(usize, 7), m.findings.len);
    try testing.expectEqual(@as(u64, 25), m.counters.inodes_scanned);
    try testing.expectEqual(@as(u64, 9), m.counters.nodes_walked);
    try testing.expectEqual(@as(u64, 1500), m.counters.scan_ms);
    try testing.expect(m.shard == null);
    try testing.expectEqual(@as(u8, 1), m.exitCode());
    // The bounded view: a cap that fits the header plus ~two findings
    // elides the rest and still says "findings".
    const empty: Report = .{ .mode = .offline, .findings = &.{}, .counters = m.counters };
    var base: std.Io.Writer.Allocating = .init(gpa);
    defer base.deinit();
    try empty.writeJson(&base.writer);
    const cap = base.written().len + 120;
    const view = try m.boundedJson(gpa, cap);
    defer gpa.free(view);
    try testing.expect(view.len <= cap);
    var pv = try Report.parse(gpa, view);
    defer pv.deinit(gpa);
    try testing.expect(pv.findings.len < 7);
    try testing.expectEqual(@as(u64, 7), pv.findings.len + pv.findings_elided);
    try testing.expect(pv.hasFindings());
    // A foreign schema and an unknown kind refuse.
    try testing.expectError(error.Corrupt, Report.parse(gpa, "{\"schema\":2,\"mode\":\"offline\",\"findings\":[]}"));
    try testing.expectError(error.Corrupt, Report.parse(gpa, "{\"schema\":1,\"mode\":\"offline\",\"findings\":[{\"kind\":\"c99_future\"}]}"));
    // The text form names the verdict.
    var txt: std.Io.Writer.Allocating = .init(gpa);
    defer txt.deinit();
    try m.writeText(&txt.writer);
    try testing.expect(std.mem.indexOf(u8, txt.written(), "findings: 7\n") != null);
    try testing.expect(std.mem.indexOf(u8, txt.written(), "[C10] ino 9 — nlink 1 BELOW 2") != null);
    const clean: Report = .{ .mode = .online, .findings = &.{} };
    try testing.expectEqual(@as(u8, 0), clean.exitCode());
    txt.clearRetainingCapacity();
    try clean.writeText(&txt.writer);
    try testing.expect(std.mem.indexOf(u8, txt.written(), "findings: 0 (clean)") != null);
}

test "a member's derived census rides its proposal: the owners round-trip through the ONE decoder (ino, every binding's volume/block/index), a malformed row refuses, and a merge carries none (an input, never an output)" {
    const gpa = testing.allocator;
    const binds_a = [_]BindingRef{ .{ .vol_tag = 0x10, .block_idx = 75, .block_index = 0 }, .{ .vol_tag = 0x20, .block_idx = 46, .block_index = 1 } };
    const binds_b = [_]BindingRef{.{ .vol_tag = 0x10, .block_idx = 75, .block_index = 0 }};
    const owners = [_]OwnerRef{ .{ .ino = key.guestLocalIno(65, 2), .bindings = &binds_a }, .{ .ino = key.guestLocalIno(65, 3), .bindings = &binds_b } };
    const r: Report = .{ .mode = .online, .findings = &.{}, .owners = &owners };
    var out: std.Io.Writer.Allocating = .init(gpa);
    defer out.deinit();
    try r.writeJson(&out.writer);
    try testing.expect(std.mem.indexOf(u8, out.written(), "\"owners\":[[") != null);
    var back = try Report.parse(gpa, out.written());
    defer back.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), back.owners.len);
    try testing.expectEqual(owners[0].ino, back.owners[0].ino);
    try testing.expectEqualSlices(BindingRef, &binds_a, back.owners[0].bindings);
    try testing.expectEqualSlices(BindingRef, &binds_b, back.owners[1].bindings);
    // The merge drops the census.
    var merged = try merge(gpa, &.{back});
    defer merged.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), merged.owners.len);
    // A row that is not `[ino, [...]]` refuses.
    try testing.expectError(error.Corrupt, Report.parse(gpa, "{\"schema\":1,\"mode\":\"online\",\"findings\":[],\"owners\":[[7]]}"));
    try testing.expectError(error.Corrupt, Report.parse(gpa, "{\"schema\":1,\"mode\":\"online\",\"findings\":[],\"owners\":[[7,[[1,2]]]]}"));
}

test "Shard.parse: k/N with 0 ≤ k < N; the residue partition is total and disjoint" {
    try testing.expectEqual(Shard{ .k = 2, .n = 5 }, Shard.parse("2/5").?);
    try testing.expect(Shard.parse("5/5") == null);
    try testing.expect(Shard.parse("0/0") == null);
    try testing.expect(Shard.parse("x/3") == null);
    try testing.expect(Shard.parse("3") == null);
    var ino: u64 = 1;
    while (ino < 1000) : (ino += 7) {
        var owners: u32 = 0;
        var k: u32 = 0;
        while (k < 4) : (k += 1) if ((Shard{ .k = k, .n = 4 }).owns(ino)) {
            owners += 1;
        };
        try testing.expectEqual(@as(u32, 1), owners);
    }
}

fn reportTextFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [2048]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    _ = Shard.parse(text);
    if (Report.parse(std.testing.allocator, text)) |*r| {
        var rr = r.*;
        defer rr.deinit(std.testing.allocator);
        // What parses prints as JSON and parses again.
        var out: std.Io.Writer.Allocating = .init(std.testing.allocator);
        defer out.deinit();
        try rr.writeJson(&out.writer);
        var again = try Report.parse(std.testing.allocator, out.written());
        again.deinit(std.testing.allocator);
    } else |_| {}
}

test "fuzz: the fsck report and shard parsers are total on arbitrary text and a parsed report re-serializes to a parseable report" {
    try fuzzkit.fuzz({}, reportTextFuzz, .{ .corpus = &.{ fuzzkit.slice("3/8"), fuzzkit.slice("{\"schema\":1}"), fuzzkit.slice("{\"schema\":99,\"findings\":[]}") } });
}
