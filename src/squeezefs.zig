//! SqueezeFS — a high-performance distributed POSIX filesystem for Linux.
//!
//! `docs/charter.md` is the specification. This file is the library root:
//! every module hangs off it, and `test` here pulls every module's tests into
//! `zig build test`. Linux-only modules compile only on Linux; the portable
//! core (codecs, derivations, knobs, counters) runs on every host so the fast
//! inner loop needs no container.

const std = @import("std");
const builtin = @import("builtin");

/// Build identity (charter §11 "Versioning"): the strings are the exe's
/// (`build_options`, imported by `main` alone), so the library never changes
/// when HEAD moves.
pub const Version = struct {
    train: []const u8,
    git_commit: []const u8,
    profile: []const u8,

    pub fn write(self: Version, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("squeezefs {s} ({s}, {s} profile, zig {s})\n", .{ self.train, self.git_commit, self.profile, builtin.zig_version_string });
    }
};

// ---- core ----
pub const checksum = @import("core/checksum.zig");
pub const sqzhash = @import("core/sqzhash.zig");
pub const clock = @import("core/clock.zig");
pub const derive = @import("core/derive.zig");
pub const rtt = @import("core/rtt.zig");
pub const core_sort = @import("core/sort.zig");
pub const knobs = @import("core/knobs.zig");
pub const log = @import("core/log.zig");
pub const errors = @import("core/errors.zig");
pub const counters = @import("core/counters.zig");
pub const json_scan = @import("core/json_scan.zig");
pub const mountinfo = @import("core/mountinfo.zig");
pub const thread_name = @import("core/thread_name.zig");
/// The test kits (compiled only under test): the ONE skip + the ONE fuzz entry.
pub const testkit = if (builtin.is_test) @import("core/testkit.zig") else struct {};
pub const fuzzkit = if (builtin.is_test) @import("core/fuzzkit.zig") else struct {};

// ---- the gates' shared code: the decoder/fuzz census the rail and the nightly campaign read (portable) ----
pub const gates = struct {
    pub const census = @import("gates/census.zig");
    pub const literals = @import("gates/literals.zig");
};

// ---- model checking: the exhaustive interleaving explorer (portable) ----
pub const model = struct {
    pub const explorer = @import("model/explorer.zig");
    pub const worker_pass_model = @import("model/worker_pass_model.zig");
};

// ---- the metadata plane: MetaLV (portable codecs + cores; device I/O via io/) ----
pub const meta = struct {
    pub const sym = @import("meta/sym.zig");
    pub const sym_slots = @import("meta/sym/slots.zig");
    pub const sym_tokens = @import("meta/sym/tokens.zig");
    pub const sym_membership = @import("meta/sym/membership.zig");
    // @anchor:sym_membership_import
    pub const guard = @import("meta/guard.zig");
    pub const routing = @import("meta/routing.zig");
    pub const meta_set = if (builtin.os.tag == .linux) @import("meta/meta_set.zig") else struct {};
    pub const conveyor = @import("meta/conveyor.zig");
    pub const device_loader = if (builtin.os.tag == .linux) @import("meta/device_loader.zig") else struct {};
    pub const backend = if (builtin.os.tag == .linux) @import("meta/backend.zig") else struct {};
    pub const tree = @import("meta/tree.zig");
    pub const merged_walk = @import("meta/merged_walk.zig");
    pub const smo = @import("meta/smo.zig");
    pub const leaf_merge = @import("meta/leaf_merge.zig");
    pub const checkpoint = @import("meta/checkpoint.zig");
    pub const node_snapshot = @import("meta/node_snapshot.zig");
    pub const node_cache = @import("meta/node_cache.zig");
    pub const economy = @import("meta/economy.zig");
    pub const journal_lane = if (builtin.os.tag == .linux) @import("meta/journal_lane.zig") else struct {};
    pub const tx = @import("meta/tx.zig");
    pub const heap_law = @import("meta/heap_law.zig");
    pub const inos = @import("meta/inos.zig");
    pub const dlm = @import("meta/dlm.zig");
    pub const key = @import("meta/key.zig");
    pub const record = @import("meta/record.zig");
    pub const bset = @import("meta/bset.zig");
    pub const journal_core = @import("meta/journal_core.zig");
    pub const journal_core_model = @import("meta/journal_core_model.zig");
    pub const journal = @import("meta/journal.zig");
    pub const ledger = @import("meta/ledger.zig");
    pub const alloc_core = @import("meta/alloc_core.zig");
    pub const alloc_ext = @import("meta/alloc_ext.zig");
    pub const superblock = @import("meta/superblock.zig");
    pub const foreign_signature = @import("meta/foreign_signature.zig");
    pub const geometry = @import("meta/geometry.zig");
    pub const node_seq = @import("meta/node_seq.zig");
    pub const node = @import("meta/node.zig");
    pub const builder = @import("meta/builder.zig");
    pub const format = @import("meta/format.zig");
    pub const node_state_core = @import("meta/node_state_core.zig");
    pub const epoch_core = @import("meta/epoch_core.zig");
    pub const conveyor_core = @import("meta/conveyor_core.zig");
};

// ---- sync primitives (charter §9), written over std.Io's futex ops ----
pub const sync = @import("sync/sync.zig");

// ---- the executor: our own std.Io implementation (decision 0001) ----
pub const exec = @import("exec/exec.zig");

// ---- the FUSE library (charter §4) ----
pub const fuse = struct {
    pub const abi = @import("fuse/abi.zig");
    pub const codec = @import("fuse/codec.zig");
    pub const geometry = @import("fuse/geometry.zig");
    pub const mount_options = @import("fuse/mount_options.zig");
    pub const transport = @import("fuse/transport.zig");
    pub const handler = @import("fuse/handler.zig");
};

// ---- the CLI verbs (charter §8; the volume verbs are Linux-only inside) ----
pub const cli = @import("cli/cli.zig");

// ---- the data plane (charter §6) ----
pub const data = struct {
    pub const sym_planes = @import("data/sym_planes.zig");
    pub const kvmap = @import("data/kvmap.zig");
    pub const layout = @import("data/layout.zig");
    pub const merge_scratch = @import("data/merge_scratch.zig");
    pub const staging = @import("data/staging.zig");
    pub const pack = @import("data/pack.zig");
    pub const router = @import("data/router.zig");
    pub const block_alloc = @import("data/block_alloc.zig");
    pub const incarnation_model = @import("data/incarnation_model.zig");
    pub const free_list_model = @import("data/free_list_model.zig");
    pub const block_refs = @import("data/block_refs.zig");
    pub const reclaim = @import("data/reclaim.zig");
    pub const enospc = @import("data/enospc.zig");
    pub const transform = @import("data/transform.zig");
    pub const set_config = @import("data/set_config.zig");
    pub const data_superblock = @import("data/data_superblock.zig");
    pub const write_admit = @import("data/write_admit.zig");
    pub const write_side_view = @import("data/write_side_view.zig");
    pub const device_overlay = @import("data/device_overlay.zig");
    pub const sole_owner_patch = @import("data/sole_owner_patch.zig");
    pub const accumulate = @import("data/accumulate.zig");
    pub const extent_overlay = @import("data/extent_overlay.zig");
    pub const write_pipeline = @import("data/write_pipeline.zig");
    pub const writeback = @import("data/writeback.zig");
    pub const size_epoch = @import("data/size_epoch.zig");
    pub const shape_table = @import("data/shape_table.zig");
    pub const fsync = @import("data/fsync.zig");
    pub const read_size_class = @import("data/read_size_class.zig");
    pub const read_fill_pool = @import("data/read_fill_pool.zig");
    pub const hot_tier = @import("data/hot_tier.zig");
    pub const read_lru = @import("data/read_lru.zig");
    pub const read_hold = @import("data/read_hold.zig");
    pub const nvme_read_cache = @import("data/nvme_read_cache.zig");
    pub const purge = @import("data/purge.zig");
    pub const single_flight_fill = @import("data/single_flight_fill.zig");
    pub const admission = @import("data/admission.zig");
    pub const prefetch = @import("data/prefetch.zig");
    pub const read_path = @import("data/read_path.zig");
    pub const warm_ladder = @import("data/warm_ladder.zig");
    pub const plane = if (builtin.os.tag == .linux) @import("data/plane.zig") else struct {};
    pub const plane_locks_model = @import("data/plane_locks_model.zig");
};

// ---- operations: fsck/scrub, the job fabric + lifecycle + defrag, nvmet targets, the zcrx lane (charter §8) ----
pub const ops = struct {
    pub const fsck = @import("ops/fsck.zig");
    pub const fsck_scale = if (builtin.os.tag == .linux) @import("ops/fsck/scale.zig") else struct {};
    pub const jobs = @import("ops/jobs.zig");
    pub const lifecycle = @import("ops/lifecycle.zig");
    pub const defrag = @import("ops/defrag.zig");
    pub const nvmet = if (builtin.os.tag == .linux) @import("ops/nvmet.zig") else struct {};
    pub const keys = @import("ops/keys.zig");
    pub const zcrx = @import("ops/zcrx.zig");
    // @anchor:ops_integrate_import
};

// ---- the client bypass: the shm session, the daemon host, the shim + SDK client (charter §7) ----
pub const ipc = struct {
    pub const session = @import("ipc/ipc.zig");
    pub const host = if (builtin.os.tag == .linux) @import("ipc/host.zig") else struct {};
    pub const client = if (builtin.os.tag == .linux) @import("ipc/client.zig") else struct {};
};

// ---- the client bypass's two clients: the LD_PRELOAD shim and the SDK (charter §7) ----
pub const shim = if (builtin.os.tag == .linux) @import("shim/shim.zig") else struct {};
// @anchor:sdk_import
// The SDK root gates its Linux half itself, so the lease law, the buddy
// allocator, the model and the version words run on every host.
pub const sdk = @import("sdk/sdk.zig");

// ---- the cluster wire (charter §5.5 verbs; §9 thread-per-connection) ----
pub const wire = @import("wire/wire.zig");

// ---- observability surface: the .stats inode + CLI families (charter §10) ----
pub const stats = @import("stats/stats.zig");

// ---- memory (portable logic; the cgroup/proc readers are Linux-only inside) ----
pub const mem_budget = @import("mem/budget.zig");
pub const process_allocator = @import("mem/process_allocator.zig");

// ---- I/O: the ring-park primitive, the io_uring file layer + metadata device posture (Linux only) ----
pub const align_probe = @import("io/align_probe.zig");
pub const ring_park = if (builtin.os.tag == .linux) @import("io/ring_park.zig") else struct {};
pub const uring128 = if (builtin.os.tag == .linux) @import("io/uring128.zig") else struct {};
pub const uring_fs = if (builtin.os.tag == .linux) @import("io/uring_fs.zig") else struct {};

// ---- I/O: NVMe block device workers (Linux only) ----
pub const nvme_derive = @import("io/nvme_derive.zig");
pub const nvme_dev = if (builtin.os.tag == .linux) @import("io/nvme_dev.zig") else struct {};

test {
    std.testing.refAllDecls(@This());
    _ = checksum;
    _ = sqzhash;
    _ = clock;
    _ = derive;
    _ = knobs;
    _ = log;
    _ = errors;
    _ = counters;
    _ = json_scan;
    _ = mountinfo;
    _ = thread_name;
    _ = testkit;
    _ = fuzzkit;
    _ = model.explorer;
    _ = model.worker_pass_model;
    _ = gates.census;
    _ = gates.literals;
    _ = ops.fsck;
    if (builtin.os.tag == .linux) _ = ops.fsck_scale;
    _ = ops.jobs;
    _ = ops.lifecycle;
    _ = ops.defrag;
    if (builtin.os.tag == .linux) _ = ops.nvmet;
    _ = ops.keys;
    _ = ops.zcrx;
    // @anchor:ops_integrate_test
    _ = ipc.session;
    if (builtin.os.tag == .linux) _ = ipc.host;
    if (builtin.os.tag == .linux) _ = shim;
    // @anchor:sdk_test
    _ = sdk;
    if (builtin.os.tag == .linux) _ = ipc.client;
    _ = meta.sym;
    _ = meta.sym_slots;
    _ = meta.sym_tokens;
    _ = meta.sym_membership;
    // @anchor:sym_membership_test
    _ = data.sym_planes;
    _ = wire;
    _ = data.kvmap;
    _ = meta.guard;
    _ = meta.routing;
    if (builtin.os.tag == .linux) _ = meta.meta_set;
    _ = stats;
    _ = data.layout;
    _ = data.staging;
    _ = data.pack;
    _ = data.router;
    _ = data.block_alloc;
    _ = data.incarnation_model;
    _ = data.free_list_model;
    _ = data.block_refs;
    _ = data.reclaim;
    _ = data.enospc;
    _ = data.transform;
    _ = data.set_config;
    _ = data.data_superblock;
    _ = data.write_admit;
    _ = data.write_side_view;
    _ = data.device_overlay;
    _ = data.sole_owner_patch;
    _ = data.accumulate;
    _ = data.extent_overlay;
    _ = data.write_pipeline;
    _ = data.writeback;
    _ = data.size_epoch;
    _ = data.shape_table;
    _ = data.fsync;
    _ = data.read_size_class;
    _ = data.read_fill_pool;
    _ = data.hot_tier;
    _ = data.read_lru;
    _ = data.read_hold;
    _ = data.nvme_read_cache;
    _ = data.purge;
    _ = data.single_flight_fill;
    _ = data.admission;
    _ = data.prefetch;
    _ = data.read_path;
    _ = data.warm_ladder;
    if (builtin.os.tag == .linux) _ = data.plane;
    _ = data.plane_locks_model;
    _ = exec;
    _ = fuse.abi;
    _ = fuse.codec;
    _ = fuse.geometry;
    _ = fuse.mount_options;
    _ = fuse.transport;
    _ = fuse.handler;
    _ = cli;
    if (builtin.os.tag == .linux) _ = uring128;
    _ = meta.conveyor;
    if (builtin.os.tag == .linux) _ = meta.device_loader;
    if (builtin.os.tag == .linux) _ = meta.backend;
    _ = meta.tree;
    _ = meta.merged_walk;
    _ = meta.smo;
    _ = meta.leaf_merge;
    _ = meta.checkpoint;
    _ = meta.node_snapshot;
    _ = meta.node_cache;
    _ = meta.economy;
    _ = sync;
    if (builtin.os.tag == .linux) _ = meta.journal_lane;
    _ = meta.tx;
    _ = meta.heap_law;
    _ = meta.inos;
    _ = meta.dlm;
    _ = meta.key;
    _ = meta.record;
    _ = meta.bset;
    _ = meta.journal_core;
    _ = meta.journal_core_model;
    _ = meta.journal;
    _ = meta.ledger;
    _ = meta.alloc_core;
    _ = meta.alloc_ext;
    _ = meta.superblock;
    _ = meta.foreign_signature;
    _ = meta.geometry;
    _ = meta.node_seq;
    _ = meta.node;
    _ = meta.builder;
    _ = meta.format;
    _ = meta.node_state_core;
    _ = meta.epoch_core;
    _ = meta.conveyor_core;
    _ = core_sort;
    _ = mem_budget;
    _ = process_allocator;
    _ = align_probe;
    if (builtin.os.tag == .linux) _ = ring_park;
    if (builtin.os.tag == .linux) _ = uring_fs;
    _ = nvme_derive;
    if (builtin.os.tag == .linux) _ = nvme_dev;
}
