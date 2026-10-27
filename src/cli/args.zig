//! The CLI grammar (charter §8): `format`, `mount`, `umount`, `status`,
//! `clients`, `df`, `bench`, `clone`, `tune`, `config`, `claim
//! clear`, `--version`, `--help`, parsed into ONE typed `Verb`. Flags and
//! positionals mix freely; `--` ends the flags; a value rides `--flag VALUE`
//! or `--flag=VALUE` (`-o OPTS` / `-oOPTS`). Every integer goes through the
//! ONE knob parser (`knobs.parseIntIn`: inclusive range, refuse never clamp)
//! and every URI through the ONE URI parser (`uri.zig`). A refusal is typed
//! and formats as one line naming the offending word and the fix; the
//! caller prints it with the verb's usage and exits 2.
//!
//! Portable; pure over argv slices (the `Verb` borrows them).

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const knobs = @import("../core/knobs.zig");
const meta_geometry = @import("../meta/geometry.zig");
const transform = @import("../data/transform.zig");
const dir_place = @import("../meta/routing/dir_place.zig");
const set_config = @import("../data/set_config.zig");
const uri = @import("uri.zig");

/// `--meta-journal-mb`: the ring override in MiB. Floor 1 MiB: the plan's
/// own floor is 384 KiB (`geometry.journal_override_floor`) and a whole
/// MiB is the unit the flag names. Ceiling 1024: the derived clamp tops at
/// 32 MiB on a 2 GiB+ volume (charter §5.1), so an override past 1 GiB is
/// 32× any derivation — a typo, never a posture.
pub const JOURNAL_MB_MIN: u32 = 1;
pub const JOURNAL_MB_MAX: u32 = 1024;
/// `--meta-node-kib`: the five powers of two `geometry.validateNodeKib`
/// admits; the integer range brackets them so the ONE parser's message
/// names a sane window before the exact-set check names the five.
pub const NODE_KIB_MIN: u32 = meta_geometry.allowed_node_kib[0];
pub const NODE_KIB_MAX: u32 = meta_geometry.allowed_node_kib[meta_geometry.allowed_node_kib.len - 1];

pub const Format = struct {
    /// Raw `sqmeta://` word (parsed by `uri`; kept raw so the verb can
    /// re-print it).
    meta_uri: []const u8,
    /// Raw `sqdata://` words, in order.
    data_uris: []const []const u8,
    single_writer: bool = false,
    /// Bytes; null = the shipped default node.
    node_size: ?u64 = null,
    /// Bytes; null = the derived clamp.
    journal_len: ?u64 = null,
    force: bool = false,
    /// `--block-size BYTES`: the data block, a power of two in
    /// [`transform.MIN_BLOCK_SIZE`, `transform.MAX_BLOCK_SIZE`]; the
    /// charter's 4 MiB when absent.
    block_size: u32 = transform.DEFAULT_BLOCK_SIZE,
    /// `--compress none|lz4|zstd` (charter §6.5).
    compression: transform.Compression = .none,
    /// `--encrypt none|aes-256-gcm|chacha20-poly1305`; requires
    /// `--encrypt-key`.
    encryption: transform.Encryption = .none,
    /// `--encrypt-key PATH|-`: the operator key file (stdin as `-`).
    encrypt_key: ?[]const u8 = null,
    /// `--dir-spread-depth N` (`0 … dir_place.DEPTH_MAX`): the set's
    /// directory spread depth (charter §5.4; decision 0003 §BZ) — the
    /// Lustre `lfs setdirstripe -D -i -1 --max-inherit-rr=N` of this
    /// format; `config set-dir-spread-depth` changes it later.
    dir_spread_depth: u8 = dir_place.DEFAULT_DEPTH,
};

pub const Mount = struct {
    meta_uri: []const u8,
    mountpoint: []const u8,
    /// The raw `-o` string (the mount verb parses it over the env TTLs).
    options: ?[]const u8 = null,
    daemon: bool = false,
    supervise: bool = false,
    log_file: ?[]const u8 = null,
    /// `--join host:port`: the live manager's listener when the durable
    /// `endpoint:` record is not to be trusted (a manager that moved its
    /// listener before its checkpoint landed).
    join: ?[]const u8 = null,
    /// `--encrypt-key PATH|-`: the operator key (resolution rung 1); read
    /// in the invoking process BEFORE a `--daemon` fork (`ops/keys.zig`).
    encrypt_key: ?[]const u8 = null,
};

/// `nvmeof` (charter §8; `docs/porting/ops_nvmet_keys.md` §1–§4): the
/// target verbs over the kernel nvmet, the initiator, the PR report.
pub const Transport = enum { tcp, loop };

/// Repeatable flags are collected in place (`--ip`, `--allow-host`): the
/// `Verb` borrows argv and owns no heap.
pub const MAX_REPEAT: usize = 16;

pub const RepeatList = struct {
    items: [MAX_REPEAT][]const u8 = undefined,
    n: usize = 0,

    pub fn slice(self: *const RepeatList) []const []const u8 {
        return self.items[0..self.n];
    }
};

pub const NvmeofShare = struct {
    backing: []const u8,
    subnqn: ?[]const u8 = null,
    transport: Transport = .tcp,
    ips: RepeatList = .{},
    port: u16 = 4420,
    allow_hosts: RepeatList = .{},
    create_size: ?u64 = null,
    /// `--ns-uuid 8-4-4-4-12`: the identity to re-present.
    ns_uuid: ?[16]u8 = null,
};

pub const NvmeofConnect = struct {
    subnqn: []const u8,
    transport: Transport = .tcp,
    ip: ?[]const u8 = null,
    port: u16 = 4420,
    hostnqn: ?[]const u8 = null,
    hostid: ?[]const u8 = null,
    host_traddr: ?[]const u8 = null,
    host_iface: ?[]const u8 = null,
    nr_io_queues: ?u32 = null,
    /// `--ns-uuid`: verify the namespace by identity.
    ns_uuid: ?[16]u8 = null,
};

pub const NvmeofTarget = union(enum) {
    status: struct { json: bool = false },
    setup,
    systemd_unit,
    stop,
};

pub const Nvmeof = union(enum) {
    share: NvmeofShare,
    unshare: struct { subnqn: []const u8 },
    list: struct { json: bool = false },
    /// `status [--json]`: every fabric subsystem of the host with its
    /// multipath head, its iopolicy and its paths (controller, address,
    /// state, identity).
    status: struct { json: bool = false },
    restore,
    adopt: struct { subnqn: []const u8 },
    target: NvmeofTarget,
    connect: NvmeofConnect,
    disconnect: struct { subnqn: []const u8, identity_only: bool = false, hostnqn: ?[]const u8 = null, hostid: ?[]const u8 = null },
    resv_report: struct { device: []const u8, json: bool = false, pr_matrix: bool = false, peer: ?[]const u8 = null, force: bool = false },
};

/// `storage {list, validate}` (§6): the host inventory and the classifier.
pub const Storage = union(enum) {
    list: struct { json: bool = false },
    validate: struct { path: []const u8 },
};

/// `key {install, show, check}` (§7.2): the key store; `key {tls-init,
/// tls-issue}`: the cluster wire's own PKI (decision 0003 §P).
pub const Key = union(enum) {
    install: struct { meta_uri: []const u8, source: []const u8, force: bool = false },
    show: struct { meta_uri: []const u8 },
    check: struct { meta_uri: []const u8, encrypt_key: ?[]const u8 = null },
    /// Mint the cluster CA into `<dir>/ca.pem` + `<dir>/ca.key`.
    tls_init: struct { dir: []const u8, name: ?[]const u8 = null, force: bool = false },
    /// Issue `<out|dir>/node.pem` + `node.key` for `--node NAME` under `<dir>`'s CA.
    tls_issue: struct { dir: []const u8, node: []const u8, out: ?[]const u8 = null, force: bool = false },
};

pub const Umount = struct {
    mountpoint: []const u8,
    /// `--lazy` / `-l`: detach a BUSY mount now and let the daemon serve
    /// its holders until the kernel ends the connection (decision 0003
    /// §S shape b); without it a held mount is refused `EBUSY` by name.
    lazy: bool = false,
};

/// A `sqmeta://` URI or a mountpoint (resolved through the mount's
/// `.config`): the ONE positional every set-level verb takes
/// (`docs/porting/stats_cli.md` §2 — no `-g`, no env default, no alias).
pub const Target = []const u8;

pub const Status = struct {
    target: Target,
};

pub const Clients = struct {
    target: Target,
    json: bool = false,
};

pub const Df = struct {
    target: Target,
    json: bool = false,
};

pub const Clone = struct {
    target: Target,
    /// Absolute paths INSIDE the set (`/dir/file`).
    src: []const u8,
    dest: []const u8,
};

pub const Tune = struct {
    apply: bool = false,
};

pub const Config = union(enum) {
    show: struct { target: Target, json: bool = false },
    /// `set-fabric-endpoints <sqmeta-uri> vol-<tag>=<trtype>:<traddr>:<trsvcid>:<subnqn>[,<path>…]…`
    /// (`ops_nvmet_keys.md` §3.7: the per-data-volume `FabricEndpoint`
    /// inside `set_config` — one subsystem, one or more paths).
    set_fabric_endpoints: struct { target: Target, specs: RepeatList },
    get_fabric_endpoints: struct { target: Target, json: bool = false },
    /// `set-dir-spread-depth N <sqmeta-uri>`: the set's directory spread
    /// depth for every directory made from the next mount on.
    set_dir_spread_depth: struct { target: Target, depth: u8 },
};

pub const ClaimClear = struct {
    meta_uri: []const u8,
};

/// `squeezefs appenders [--json] sqmeta://<path>`.
pub const Appenders = struct {
    meta_uri: []const u8,
    json: bool = false,
};

/// `squeezefs appender clear sqmeta://<path> <id>` (ids ≥ 1; 0 refuses at
/// the verb — a dead manager's region is the D0 successor's).
pub const AppenderClear = struct {
    meta_uri: []const u8,
    appender_id: u32,
};

/// `--take-slots k | s1,s2,…` (`add-meta`): a count or an explicit list.
pub const TakeSlots = union(enum) {
    count: u32,
    /// The raw comma list; the verb parses each member as a slot id.
    list: []const u8,
};

/// `volume {list, add-meta, remove-meta, repair-set, enable-symmetric,
/// migrate-meta-slot}` (charter §8; `docs/porting/dynamic_routing.md` §3,
/// §4; §5.4): every verb takes the set's `sqmeta://` URI (`add-meta`/
/// `remove-meta` name the device too); the online engine's verb takes the
/// slot and the target member.
pub const Volume = union(enum) {
    list: struct { meta_uri: []const u8 },
    add_meta: struct { meta_uri: []const u8, device: []const u8, take: TakeSlots },
    remove_meta: struct { meta_uri: []const u8, victim: []const u8 },
    repair_set: struct { meta_uri: []const u8 },
    /// The flat `--single-writer` class → the symmetric plane, offline.
    enable_symmetric: struct { meta_uri: []const u8 },
    migrate_meta_slot: struct { meta_uri: []const u8, slot: u16, target: u16 },
    // @anchor:ops_jobs_volume_verbs — the data-volume lifecycle (rung 8).
    add_data: struct { meta_uri: []const u8, device: []const u8, no_rebalance: bool },
    remove_data: struct { meta_uri: []const u8, volume: []const u8 },
    undrain: struct { meta_uri: []const u8, volume: []const u8 },
};

/// `job {list, status, pause, resume, cancel, throttle, worker,
/// rotate-enroll}` (charter §8; `ops_jobs_lifecycle.md` §1.10): the
/// durable-record probes take the `sqmeta://` set; the live controls the
/// mountpoint's admin lane; `worker` dials a set's coordinator.
pub const Job = union(enum) {
    list: struct { target: []const u8, json: bool },
    status: struct { target: []const u8, job_id: u64 },
    pause: struct { target: []const u8, job_id: u64 },
    @"resume": struct { target: []const u8, job_id: u64 },
    cancel: struct { target: []const u8, job_id: u64 },
    throttle: struct { target: []const u8, job_id: u64, pct: u8 },
    worker: struct { meta_uri: []const u8 },
    rotate_enroll: struct { meta_uri: []const u8 },
};

/// `defrag TARGET [--data] [--pack] [--volume ID] [--meta] [--fold]
/// [--rebalance] [--report-only] [--throttle PCT] [--json]` (§3.3).
pub const Defrag = struct {
    target: []const u8,
    data: bool = false,
    pack: bool = false,
    volume: ?u64 = null,
    meta: bool = false,
    fold: bool = false,
    rebalance: bool = false,
    report_only: bool = false,
    /// `--rekey --encrypt-key <path|->`: the key-rotation sweep on a LIVE
    /// mount — exclusive of every other arm (`ops/defrag/rekey.zig`).
    rekey: bool = false,
    encrypt_key: ?[]const u8 = null,
    throttle: u8 = 100,
    json: bool = false,

    pub fn anyArm(self: Defrag) bool {
        return self.data or self.pack or self.meta or self.fold or self.rebalance or self.report_only or self.rekey;
    }
};

/// `bench` (stats_cli.md §3): the shape vocabulary `threads × files × size
/// @ block`; null = auto-derived from the mount's geometry; the bare
/// invocation (no phase flag) runs the saturation suite.
pub const Bench = struct {
    mountpoint: []const u8,
    write: bool = false,
    read: bool = false,
    stat: bool = false,
    del: bool = false,
    threads: ?u32 = null,
    files: ?u32 = null,
    size: ?u64 = null,
    block: ?u64 = null,
    rand: bool = false,
    direct: bool = false,
    /// Seconds; null = the default per access pattern; 0 = full coverage.
    time: ?u32 = null,
    iterations: u32 = 1,
    /// One iteration of every workload on a tiny shape (the gate).
    smoke: bool = false,
    /// A-B-B-A over the one lever the client controls (O_DIRECT vs
    /// buffered): A = `--direct` as given, B = its opposite.
    abba: bool = false,
    json: bool = false,
    /// The hash microbench (`bench.hash_bench`): the format's hash against
    /// the xxh3-64 reference column on the format's shapes; CPU only, no
    /// mountpoint (the one `bench` form without one).
    hash: bool = false,

    pub fn anyPhase(self: Bench) bool {
        return self.write or self.read or self.stat or self.del;
    }
};

// ---- @anchor:ops_fsck_args — `fsck` / `scrub` / `fsck merge-reports` (charter §8; `ops_fsck.md` §6) ----

/// `fsck [--online|--offline] [--json] [--scrub] [--throttle PCT]
/// [--shards k/N] [--repair [--apply] [--quarantine-dir DIR]] TARGET` and
/// `scrub …` (= `--scrub-only`). The mode defaults by the target's kind: a
/// `sqmeta://` URI is offline, a mountpoint online. Dry-run is the default
/// under `--repair`; `--apply` executes.
pub const Fsck = struct {
    target: Target,
    /// Explicit `--online` / `--offline`; null = by the target's kind.
    mode: ?FsckMode = null,
    json: bool = false,
    scrub: bool = false,
    /// The `scrub` verb: C7 alone.
    scrub_only: bool = false,
    /// 0 / 100 = unthrottled.
    throttle: u8 = 0,
    /// `--shards k/N`: the residue shard (offline, or online through the
    /// admin lane's `admin_fsck`); validated here.
    shards: ?FsckShard = null,
    repair: bool = false,
    apply: bool = false,
    quarantine_dir: ?[]const u8 = null,

    pub fn offline(self: Fsck) bool {
        return if (self.mode) |m| m == .offline else !targetIsMountpoint(self.target);
    }
};

pub const FsckMode = enum { online, offline };
pub const FsckShard = struct { k: u32, n: u32 };

/// `fsck merge-reports [--json] FILE…`: durable shard reports merged.
pub const FsckMerge = struct {
    files: []const []const u8,
    json: bool = false,
};

pub const Verb = union(enum) {
    version,
    help,
    format: Format,
    mount: Mount,
    umount: Umount,
    status: Status,
    clients: Clients,
    df: Df,
    bench: Bench,
    clone: Clone,
    tune: Tune,
    config: Config,
    claim_clear: ClaimClear,
    appenders: Appenders,
    appender_clear: AppenderClear,
    volume: Volume,
    fsck: Fsck,
    fsck_merge: FsckMerge,
    job: Job,
    defrag: Defrag,
    nvmeof: Nvmeof,
    storage: Storage,
    key: Key,
};

pub const Refusal = union(enum) {
    no_verb,
    unknown_verb: []const u8,
    unknown_flag: struct { verb: []const u8, flag: []const u8 },
    missing_value: struct { verb: []const u8, flag: []const u8 },
    /// An integer flag through the ONE parser.
    bad_int: knobs.Diagnostic,
    bad_node_kib: u32,
    bad_uri: uri.Refusal,
    missing_positional: struct { verb: []const u8, what: []const u8 },
    extra_positional: struct { verb: []const u8, word: []const u8 },
    /// Not `vol-<16 hex>` / `job-<16 hex>` (rung 8's verbs).
    bad_volume_id: []const u8,
    bad_job_id: []const u8,
    supervise_without_daemon,
    log_file_without_daemon,
    daemon_without_log_file,
    /// `--block-size` outside the power-of-two window.
    bad_block_size: u64,
    bad_dir_spread_depth: []const u8,
    bad_compression: []const u8,
    bad_encryption: []const u8,
    /// `--encrypt` names an algorithm but no `--encrypt-key` was given.
    encrypt_without_key,
    /// `defrag --rekey` without `--encrypt-key`, `--encrypt-key` without
    /// `--rekey`, `--rekey` beside another arm, or `--rekey` on a set URI.
    rekey_shape: enum { no_key, key_without_rekey, other_arm, offline },
    /// `--encrypt-key` on a plaintext set.
    key_without_encrypt,
    /// A set-level target must be a `sqmeta://` URI or an absolute mountpoint.
    bad_target: []const u8,
    /// A clone path must be absolute inside the set.
    bad_set_path: []const u8,
    /// `config`/`claim` need a sub-verb.
    missing_subverb: []const u8,
    unknown_subverb: struct { verb: []const u8, word: []const u8 },
    /// `bench --hash`: a phase, shape or bracket flag beside the CPU bench.
    bench_hash_with_shape,
    /// `bench`: a size word that is not `<int>[k|m|g|t]`.
    bad_size: struct { flag: []const u8, word: []const u8 },
    bench_suite_with_shape: []const u8,
    /// `--take-slots` neither a count nor a comma list of slot ids.
    bad_take_slots: []const u8,
    /// fsck's argument lattice (`ops_fsck.md` §6): the offending shape.
    fsck_lattice: []const u8,
    /// `--shards` is not `k/N` with `0 ≤ k < N`.
    bad_shards: []const u8,
    /// `--transport` outside tcp|loop.
    bad_transport: []const u8,
    /// A repeatable flag past `MAX_REPEAT`.
    too_many_repeats: []const u8,
    /// `--ns-uuid` not `8-4-4-4-12` hex.
    bad_uuid: []const u8,
    /// `connect --hostnqn` without `--hostid` or the reverse.
    identity_half: []const u8,
    /// `--pr-matrix` without `--peer`.
    matrix_without_peer,
    /// A `vol-<tag>=<trtype>:<traddr>:<trsvcid>:<subnqn>` spec that does not parse.
    bad_endpoint_spec: struct { word: []const u8, why: EndpointSpecRefusal },

    pub fn format(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .no_verb => try w.writeAll("no verb given"),
            .unknown_verb => |v| try w.print("'{s}' is not a squeezefs verb", .{v}),
            .unknown_flag => |f| try w.print("{s}: unknown flag '{s}'", .{ f.verb, f.flag }),
            .missing_value => |f| try w.print("{s}: '{s}' requires a value", .{ f.verb, f.flag }),
            .bad_int => |d| try w.print("{f}", .{d}),
            .bad_node_kib => |kib| try meta_geometry.writeNodeKibRefusal(w, kib),
            .bad_uri => |r| try w.print("{f}", .{r}),
            .missing_positional => |m| try w.print("{s}: missing {s}", .{ m.verb, m.what }),
            .extra_positional => |e| try w.print("{s}: unexpected argument '{s}'", .{ e.verb, e.word }),
            .supervise_without_daemon => try w.writeAll("mount: --supervise requires --daemon (the supervisor is the daemon's parent process)"),
            .log_file_without_daemon => try w.writeAll("mount: --log-file requires --daemon (a foreground mount logs to stderr)"),
            .daemon_without_log_file => try w.writeAll("mount: --daemon requires --log-file PATH (a detached daemon must log somewhere; the invoking shell's streams are released at ready)"),
            .bad_block_size => |b| try w.print("format: --block-size {d} is not a power of two in [{d}, {d}]", .{ b, transform.MIN_BLOCK_SIZE, transform.MAX_BLOCK_SIZE }),
            .bad_dir_spread_depth => |w0| try w.print("config set-dir-spread-depth: '{s}' is not a depth in 0 … {d}", .{ w0, dir_place.DEPTH_MAX }),
            .bad_compression => |c| try w.print("format: --compress '{s}' is not one of none|lz4|zstd", .{c}),
            .bad_encryption => |e| try w.print("format: --encrypt '{s}' is not one of none|aes-256-gcm|chacha20-poly1305", .{e}),
            .encrypt_without_key => try w.writeAll("format: --encrypt names an algorithm but no key was given — pass --encrypt-key <path|-> (≥ 32 bytes of key material, mode 0600)"),
            .key_without_encrypt => try w.writeAll("format: --encrypt-key was given for a plaintext set — pass --encrypt aes-256-gcm|chacha20-poly1305 or drop the key"),
            .rekey_shape => |r| switch (r) {
                .no_key => try w.writeAll("defrag --rekey: the NEW key is required — pass --encrypt-key <path|-> (≥ 32 bytes of key material, mode 0600)"),
                .key_without_rekey => try w.writeAll("defrag: --encrypt-key belongs to --rekey (the key-rotation sweep) — pass --rekey or drop the key"),
                .other_arm => try w.writeAll("defrag --rekey runs alone: every block of the set is rewritten onto the new key; run the other arms in their own invocation"),
                .offline => try w.writeAll("defrag --rekey runs on a LIVE mount (the sweep is the mount's fabric; the window opens in its config): pass the mountpoint, not the sqmeta:// set"),
            },
            .bad_target => |t| try w.print("'{s}' is neither a sqmeta://<absolute path>[,…] URI nor an absolute mountpoint", .{t}),
            .bad_set_path => |t| try w.print("clone: '{s}' is not an absolute path inside the set (`/dir/file`)", .{t}),
            .missing_subverb => |v| try w.print("{s}: missing the sub-verb", .{v}),
            .unknown_subverb => |u| try w.print("{s}: '{s}' is not a sub-verb", .{ u.verb, u.word }),
            .bad_size => |b| try w.print("bench: {s} '{s}' is not a size (<integer>[k|m|g|t], ≥ 1)", .{ b.flag, b.word }),
            .bench_suite_with_shape => |f| try w.print("bench: '{s}' needs a phase flag (-w/-r/--stat/--del) — the bare invocation runs the fixed saturation suite", .{f}),
            .bench_hash_with_shape => try w.writeAll("bench: --hash is the CPU hash microbench — it takes no mountpoint, phase, shape or bracket flag (only --json)"),
            .bad_take_slots => |t| try w.print("volume add-meta: --take-slots '{s}' is neither a count (≥ 1) nor a comma list of slot ids (each < 65536)", .{t}),
            .fsck_lattice => |why| try w.print("fsck: {s}", .{why}),
            .bad_shards => |t| try w.print("fsck: --shards '{s}' is not k/N with 0 ≤ k < N", .{t}),
            .bad_volume_id => |v| try w.print("'{s}' is not a volume id (vol-<16 hex>)", .{v}),
            .bad_job_id => |j| try w.print("'{s}' is not a job id (job-<16 hex>, never zero)", .{j}),
            .bad_transport => |t| try w.print("--transport '{s}' is not one of tcp|loop", .{t}),
            .too_many_repeats => |f| try w.print("{s} given more than {d} times", .{ f, MAX_REPEAT }),
            .bad_uuid => |u| try w.print("--ns-uuid '{s}' is not a uuid (8-4-4-4-12 hex)", .{u}),
            .identity_half => |f| try w.print("nvmeof connect: {s} given without its pair — hostnqn and hostid are PAIR-OR-NEITHER", .{f}),
            .matrix_without_peer => try w.writeAll("nvmeof resv-report: --pr-matrix needs --peer <second namespace node> (a second association under a DISTINCT host identity)"),
            .bad_endpoint_spec => |s| switch (s.why) {
                .shape => try w.print("config set-fabric-endpoints: '{s}' is not vol-<16 hex>=<tcp|loop>:<traddr>:<trsvcid>:<subnqn>[,<tcp|loop>:<traddr>:<trsvcid>:<subnqn>…]", .{s.word}),
                .subnqn_differs => |d| try w.print("config set-fabric-endpoints: '{s}' names two subsystems, '{s}' and '{s}' — every path of a volume reaches ONE subsystem (its paths merge under one multipath head); a second subsystem is another volume", .{ s.word, d.a, d.b }),
                .repeated_path => |p| try w.print("config set-fabric-endpoints: '{s}' names the path '{s}' twice (the kernel answers a repeated connect EALREADY)", .{ s.word, p }),
                .too_many_paths => try w.print("config set-fabric-endpoints: '{s}' names more than {d} paths (the `share` verb's listener cap)", .{ s.word, MAX_REPEAT }),
            },
        }
    }
};

pub const Error = error{Refused};

/// A verb's usage, one line per form.
pub fn usage(verb: ?std.meta.Tag(Verb)) []const u8 {
    const all =
        \\usage: squeezefs <verb> [flags] …
        \\  squeezefs format [--single-writer] [--meta-node-kib N] [--meta-journal-mb N] [--dir-spread-depth N] [--block-size BYTES] [--compress none|lz4|zstd] [--encrypt none|aes-256-gcm|chacha20-poly1305 --encrypt-key PATH|-] [--force] sqmeta://<path> [sqdata://<path>…]
        \\  squeezefs mount [-o opt[,opt…]] [--daemon --log-file PATH [--supervise]] [--join host:port] [--encrypt-key PATH|-] sqmeta://<path> <mountpoint>
        \\  squeezefs umount [--lazy|-l] <mountpoint>
        \\  squeezefs status sqmeta://<path>|<mountpoint>
        \\  squeezefs clients [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs df [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs clone sqmeta://<path>|<mountpoint> /src/file /dest/file
        \\  squeezefs tune [--apply]
        \\  squeezefs config show [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs config set-dir-spread-depth N sqmeta://<path>
        \\  squeezefs claim clear sqmeta://<path>
        \\  squeezefs appenders [--json] sqmeta://<path>
        \\  squeezefs appender clear sqmeta://<path> <id>
        \\  squeezefs volume list sqmeta://<path>[,…]
        \\  squeezefs volume add-meta --take-slots K|s1,s2,… sqmeta://<path>[,…] <device>
        \\  squeezefs volume remove-meta sqmeta://<path>[,…] <victim device>
        \\  squeezefs volume repair-set sqmeta://<path>[,…]
        \\  squeezefs volume enable-symmetric sqmeta://<path>[,…]
        \\  squeezefs volume migrate-meta-slot sqmeta://<path>[,…] <slot> <target member index>
        \\  squeezefs fsck [--online|--offline] [--json] [--scrub] [--throttle PCT] [--shards k/N] [--repair [--apply] [--quarantine-dir DIR]] sqmeta://<path>|<mountpoint>
        \\  squeezefs scrub [--online|--offline] [--json] [--throttle PCT] sqmeta://<path>|<mountpoint>
        \\  squeezefs fsck merge-reports [--json] <report.json>…
        \\  squeezefs volume add-data [--no-rebalance] sqmeta://<path>[,…]|<mountpoint> <device>
        \\  squeezefs volume remove-data sqmeta://<path>[,…] vol-<16 hex>
        \\  squeezefs volume undrain sqmeta://<path>[,…] vol-<16 hex>
        \\  squeezefs job list [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs job status|pause|resume|cancel sqmeta://<path>|<mountpoint> job-<16 hex>
        \\  squeezefs job throttle <mountpoint> job-<16 hex> <pct>
        \\  squeezefs job worker sqmeta://<path>
        \\  squeezefs job rotate-enroll sqmeta://<path>
        \\  squeezefs defrag [--data] [--pack] [--volume vol-<16 hex>] [--meta] [--fold] [--rebalance] [--report-only] [--throttle PCT] [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs defrag --rekey --encrypt-key <path|-> [--throttle PCT] <mountpoint>
        \\  squeezefs bench [-w] [-r] [--stat] [--del] [-t N] [-n N] [-s SIZE] [-b SIZE] [--rand] [--direct] [--time S] [-i N] [--abba] [--json] [--smoke] <mountpoint>
        \\  squeezefs bench --hash [--json]
        \\  squeezefs nvmeof share [--subnqn NQN] [--transport tcp|loop] [--ip ADDR]… [--port N] [--allow-host NQN]… [--create-size SIZE] [--ns-uuid UUID] <backing>
        \\  squeezefs nvmeof unshare <subnqn>
        \\  squeezefs nvmeof list [--json]
        \\  squeezefs nvmeof status [--json]
        \\  squeezefs nvmeof restore
        \\  squeezefs nvmeof adopt <subnqn>
        \\  squeezefs nvmeof target status [--json] | setup | systemd-unit | stop
        \\  squeezefs nvmeof connect [--transport tcp|loop] [--ip ADDR] [--port N] [--hostnqn NQN --hostid UUID] [--host-traddr ADDR] [--host-iface IF] [--nr-io-queues N] [--ns-uuid UUID] <subnqn>
        \\  squeezefs nvmeof disconnect [--identity-only [--hostnqn NQN --hostid UUID]] <subnqn>
        \\  squeezefs nvmeof resv-report [--json] [--pr-matrix --peer <dev> [--force]] <dev>
        \\  squeezefs storage list [--json]
        \\  squeezefs storage validate <path>
        \\  squeezefs key install [--force] sqmeta://<path> <keyfile|->
        \\  squeezefs key show sqmeta://<path>
        \\  squeezefs key check [--encrypt-key PATH|-] sqmeta://<path>
        \\  squeezefs config set-fabric-endpoints sqmeta://<path> vol-<tag>=<tcp|loop>:<traddr>:<trsvcid>:<subnqn>[,<path>…]…
        \\  squeezefs config get-fabric-endpoints [--json] sqmeta://<path>|<mountpoint>
        \\  squeezefs --version
        \\
    ;
    const v = verb orelse return all;
    return switch (v) {
        .format => "usage: squeezefs format [--single-writer] [--meta-node-kib 64|128|256|512|1024] [--meta-journal-mb N] [--dir-spread-depth 0…254] [--block-size BYTES] [--compress none|lz4|zstd] [--encrypt none|aes-256-gcm|chacha20-poly1305 --encrypt-key PATH|-] [--force] sqmeta://<path> [sqdata://<path>…]\n",
        .mount => "usage: squeezefs mount [-o opt[,opt…]] [--daemon --log-file PATH [--supervise]] [--join host:port] [--encrypt-key PATH|-] sqmeta://<path> <mountpoint>\n",
        .nvmeof => "usage: squeezefs nvmeof share [--subnqn NQN] [--transport tcp|loop] [--ip ADDR]… [--port N] [--allow-host NQN]… [--create-size SIZE] [--ns-uuid UUID] <backing>\n       squeezefs nvmeof unshare <subnqn>\n       squeezefs nvmeof list [--json]\n       squeezefs nvmeof status [--json]\n       squeezefs nvmeof restore\n       squeezefs nvmeof adopt <subnqn>\n       squeezefs nvmeof target status [--json] | setup | systemd-unit | stop\n       squeezefs nvmeof connect [--transport tcp|loop] [--ip ADDR] [--port N] [--hostnqn NQN --hostid UUID] [--host-traddr ADDR] [--host-iface IF] [--nr-io-queues N] [--ns-uuid UUID] <subnqn>\n       squeezefs nvmeof disconnect [--identity-only [--hostnqn NQN --hostid UUID]] <subnqn>\n       squeezefs nvmeof resv-report [--json] [--pr-matrix --peer <dev> [--force]] <dev>\n",
        .storage => "usage: squeezefs storage list [--json]\n       squeezefs storage validate <path>\n",
        .key => "usage: squeezefs key install [--force] sqmeta://<path> <keyfile|->\n       squeezefs key show sqmeta://<path>\n       squeezefs key check [--encrypt-key PATH|-] sqmeta://<path>\n       squeezefs key tls-init [--name CA-NAME] [--force] <dir>\n       squeezefs key tls-issue --node NAME [--out DIR] [--force] <ca-dir>\n",

        .umount => "usage: squeezefs umount [--lazy|-l] <mountpoint>\n",
        .status => "usage: squeezefs status sqmeta://<path>|<mountpoint>\n",
        .clients => "usage: squeezefs clients [--json] sqmeta://<path>|<mountpoint>\n",
        .df => "usage: squeezefs df [--json] sqmeta://<path>|<mountpoint>\n",
        .clone => "usage: squeezefs clone sqmeta://<path>|<mountpoint> /src/file /dest/file\n",
        .tune => "usage: squeezefs tune [--apply]\n",
        .config => "usage: squeezefs config show [--json] sqmeta://<path>|<mountpoint>\n       squeezefs config set-fabric-endpoints sqmeta://<path> vol-<tag>=<tcp|loop>:<traddr>:<trsvcid>:<subnqn>[,<path>…]…\n       squeezefs config get-fabric-endpoints [--json] sqmeta://<path>|<mountpoint>\n       squeezefs config set-dir-spread-depth 0…254 sqmeta://<path>   (directories at depth ≤ N round-robin over the members; 0 = never below /'s children; the set must be unmounted; takes effect at the next mount)\n",
        .claim_clear => "usage: squeezefs claim clear sqmeta://<path>\n",
        .appenders => "usage: squeezefs appenders [--json] sqmeta://<path>\n",
        .appender_clear => "usage: squeezefs appender clear sqmeta://<path> <id>\n",
        .fsck => "usage: squeezefs fsck [--online|--offline] [--json] [--scrub] [--throttle PCT] [--shards k/N] [--repair [--apply] [--quarantine-dir DIR]] sqmeta://<path>|<mountpoint>\n       squeezefs scrub [--online|--offline] [--json] [--throttle PCT] sqmeta://<path>|<mountpoint>\n",
        .fsck_merge => "usage: squeezefs fsck merge-reports [--json] <report.json>…\n",
        .volume => "usage: squeezefs volume list sqmeta://<path>[,…]\n       squeezefs volume add-meta --take-slots K|s1,s2,… sqmeta://<path>[,…] <device>\n       squeezefs volume remove-meta sqmeta://<path>[,…] <victim device>\n       squeezefs volume repair-set sqmeta://<path>[,…]\n       squeezefs volume enable-symmetric sqmeta://<path>[,…]\n       squeezefs volume migrate-meta-slot sqmeta://<path>[,…] <slot> <target member index>\n       squeezefs volume add-data [--no-rebalance] sqmeta://<path>[,…]|<mountpoint> <device>\n       squeezefs volume remove-data sqmeta://<path>[,…]|<mountpoint> vol-<16 hex>\n       squeezefs volume undrain sqmeta://<path>[,…]|<mountpoint> vol-<16 hex>\n",
        .job => "usage: squeezefs job list [--json] sqmeta://<path>|<mountpoint>\n       squeezefs job status|pause|resume|cancel sqmeta://<path>|<mountpoint> job-<16 hex>\n       squeezefs job throttle <mountpoint> job-<16 hex> <pct>\n       squeezefs job worker sqmeta://<path>\n       squeezefs job rotate-enroll sqmeta://<path>\n",
        .defrag => "usage: squeezefs defrag [--data] [--pack] [--volume vol-<16 hex>] [--meta] [--fold] [--rebalance] [--report-only] [--throttle PCT] [--json] sqmeta://<path>|<mountpoint>\n       squeezefs defrag --rekey --encrypt-key <path|-> [--throttle PCT] <mountpoint>\n",
        .bench => "usage: squeezefs bench [-w] [-r] [--stat] [--del] [-t THREADS] [-n FILES] [-s SIZE] [-b BLOCK] [--rand] [--direct] [--time SECS] [-i ITERATIONS] [--abba] [--json] [--smoke] <mountpoint>\n       squeezefs bench --hash [--json]\n",
        .version, .help => all,
    };
}

/// A flag word's name and inline value (`--k=v` / `-ov`).
const Flag = struct {
    name: []const u8,
    inline_value: ?[]const u8,

    fn of(word: []const u8) ?Flag {
        if (word.len < 2 or word[0] != '-') return null;
        if (std.mem.startsWith(u8, word, "--")) {
            if (std.mem.indexOfScalar(u8, word, '=')) |eq| return .{ .name = word[0..eq], .inline_value = word[eq + 1 ..] };
            return .{ .name = word, .inline_value = null };
        }
        // A short flag: `-o` alone or `-o<value>` glued.
        return .{ .name = word[0..2], .inline_value = if (word.len > 2) word[2..] else null };
    }
};

/// Cursor over `argv[2..]` with the shared value/positional mechanics.
const Cursor = struct {
    verb: []const u8,
    words: []const []const u8,
    i: usize = 0,
    flags_ended: bool = false,
    positionals: [uri.MAX_MEMBERS + 2][]const u8 = undefined,
    npos: usize = 0,
    diag: *Refusal,

    fn next(self: *Cursor) ?[]const u8 {
        if (self.i >= self.words.len) return null;
        const w = self.words[self.i];
        self.i += 1;
        return w;
    }

    /// The flag's value: inline, else the next word.
    fn value(self: *Cursor, f: Flag) Error![]const u8 {
        if (f.inline_value) |v| return v;
        return self.next() orelse {
            self.diag.* = .{ .missing_value = .{ .verb = self.verb, .flag = f.name } };
            return error.Refused;
        };
    }

    fn positional(self: *Cursor, word: []const u8) Error!void {
        if (self.npos == self.positionals.len) {
            self.diag.* = .{ .extra_positional = .{ .verb = self.verb, .word = word } };
            return error.Refused;
        }
        self.positionals[self.npos] = word;
        self.npos += 1;
    }

    fn unknown(self: *Cursor, f: Flag) Error {
        self.diag.* = .{ .unknown_flag = .{ .verb = self.verb, .flag = f.name } };
        return error.Refused;
    }

    /// Classify the next word: a flag (to the verb's switch), a positional
    /// (collected), or the end.
    fn step(self: *Cursor) Error!?Flag {
        while (self.next()) |w| {
            if (!self.flags_ended) {
                if (std.mem.eql(u8, w, "--")) {
                    self.flags_ended = true;
                    continue;
                }
                if (Flag.of(w)) |f| return f;
            }
            try self.positional(w);
        }
        return null;
    }

    fn pos(self: *Cursor) []const []const u8 {
        return self.positionals[0..self.npos];
    }
};

fn parseIntFlag(c: *Cursor, f: Flag, comptime T: type, lo: T, hi: T) Error!T {
    const raw = try c.value(f);
    return switch (knobs.parseIntIn(T, f.name, raw, lo, hi)) {
        .value => |v| v,
        .absent => blk: {
            c.diag.* = .{ .missing_value = .{ .verb = c.verb, .flag = f.name } };
            break :blk error.Refused;
        },
        .invalid => |d| blk: {
            c.diag.* = .{ .bad_int = d };
            break :blk error.Refused;
        },
    };
}

fn checkUri(scheme: uri.Scheme, word: []const u8, diag: *Refusal) Error!void {
    var buf: [uri.MAX_MEMBERS][]const u8 = undefined;
    var r: uri.Refusal = undefined;
    _ = uri.parse(scheme, word, &buf, &r) catch {
        diag.* = .{ .bad_uri = r };
        return error.Refused;
    };
}

/// `argv` as the process received it (`argv[0]` is the binary).
pub fn parse(argv: []const []const u8, diag: *Refusal) Error!Verb {
    if (argv.len < 2) {
        diag.* = .no_verb;
        return error.Refused;
    }
    const verb = argv[1];
    const words = argv[2..];
    if (std.mem.eql(u8, verb, "--version") or std.mem.eql(u8, verb, "version")) return .version;
    if (std.mem.eql(u8, verb, "--help") or std.mem.eql(u8, verb, "-h") or std.mem.eql(u8, verb, "help")) return .help;
    if (std.mem.eql(u8, verb, "format")) return parseFormat(words, diag);
    if (std.mem.eql(u8, verb, "mount")) return parseMount(words, diag);
    if (std.mem.eql(u8, verb, "umount")) return parseUmount(words, diag);
    if (std.mem.eql(u8, verb, "status")) return parseStatus(words, diag);
    if (std.mem.eql(u8, verb, "clients")) return parseTargetVerb(.clients, words, diag);
    if (std.mem.eql(u8, verb, "df")) return parseTargetVerb(.df, words, diag);
    if (std.mem.eql(u8, verb, "clone")) return parseClone(words, diag);
    if (std.mem.eql(u8, verb, "tune")) return parseTune(words, diag);
    if (std.mem.eql(u8, verb, "config")) return parseConfig(words, diag);
    if (std.mem.eql(u8, verb, "claim")) return parseClaim(words, diag);
    if (std.mem.eql(u8, verb, "appenders")) return parseAppenders(words, diag);
    if (std.mem.eql(u8, verb, "appender")) return parseAppenderClear(words, diag);
    if (std.mem.eql(u8, verb, "volume")) return parseVolume(words, diag);
    if (std.mem.eql(u8, verb, "fsck")) return parseFsck(words, diag, false);
    if (std.mem.eql(u8, verb, "scrub")) return parseFsck(words, diag, true);
    if (std.mem.eql(u8, verb, "bench")) return parseBench(words, diag);
    if (std.mem.eql(u8, verb, "job")) return parseJob(words, diag);
    if (std.mem.eql(u8, verb, "defrag")) return parseDefrag(words, diag);
    if (std.mem.eql(u8, verb, "nvmeof")) return parseNvmeof(words, diag);
    if (std.mem.eql(u8, verb, "storage")) return parseStorage(words, diag);
    if (std.mem.eql(u8, verb, "key")) return parseKey(words, diag);
    diag.* = .{ .unknown_verb = verb };
    return error.Refused;
}

// ---------------------------------------------------------------------------
// nvmeof / storage / key (rung 8).
// ---------------------------------------------------------------------------

fn parseTransport(c: *Cursor, f: Flag) Error!Transport {
    const raw = try c.value(f);
    if (std.mem.eql(u8, raw, "tcp")) return .tcp;
    if (std.mem.eql(u8, raw, "loop")) return .loop;
    c.diag.* = .{ .bad_transport = raw };
    return error.Refused;
}

fn pushRepeat(c: *Cursor, f: Flag, list: *RepeatList) Error!void {
    const raw = try c.value(f);
    if (list.n == MAX_REPEAT) {
        c.diag.* = .{ .too_many_repeats = f.name };
        return error.Refused;
    }
    list.items[list.n] = raw;
    list.n += 1;
}

/// `8-4-4-4-12` hex (or 32 bare hex digits) → bytes.
pub fn parseUuid(word: []const u8) ?[16]u8 {
    var out: [16]u8 = undefined;
    var n: usize = 0;
    var hi: ?u8 = null;
    for (word) |ch| {
        if (ch == '-') continue;
        const v: u8 = switch (ch) {
            '0'...'9' => ch - '0',
            'a'...'f' => ch - 'a' + 10,
            'A'...'F' => ch - 'A' + 10,
            else => return null,
        };
        if (hi) |h| {
            if (n == 16) return null;
            out[n] = (h << 4) | v;
            n += 1;
            hi = null;
        } else hi = v;
    }
    if (hi != null or n != 16) return null;
    return out;
}

fn parseUuidFlag(c: *Cursor, f: Flag) Error![16]u8 {
    const raw = try c.value(f);
    return parseUuid(raw) orelse {
        c.diag.* = .{ .bad_uuid = raw };
        return error.Refused;
    };
}

fn parseNvmeof(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("nvmeof", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "share")) {
        var c: Cursor = .{ .verb = "nvmeof share", .words = rest, .diag = diag };
        var out: NvmeofShare = .{ .backing = undefined };
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--subnqn")) {
                out.subnqn = try c.value(f);
            } else if (std.mem.eql(u8, f.name, "--transport")) {
                out.transport = try parseTransport(&c, f);
            } else if (std.mem.eql(u8, f.name, "--ip")) {
                try pushRepeat(&c, f, &out.ips);
            } else if (std.mem.eql(u8, f.name, "--port")) {
                out.port = try parseIntFlag(&c, f, u16, 1, std.math.maxInt(u16));
            } else if (std.mem.eql(u8, f.name, "--allow-host")) {
                try pushRepeat(&c, f, &out.allow_hosts);
            } else if (std.mem.eql(u8, f.name, "--create-size")) {
                out.create_size = try parseSizeFlag(&c, f);
            } else if (std.mem.eql(u8, f.name, "--ns-uuid")) {
                out.ns_uuid = try parseUuidFlag(&c, f);
            } else return c.unknown(f);
        }
        out.backing = try exactlyOne(&c, "the backing (a regular file or block device)");
        return .{ .nvmeof = .{ .share = out } };
    }
    if (std.mem.eql(u8, sub, "unshare") or std.mem.eql(u8, sub, "adopt")) {
        const is_unshare = std.mem.eql(u8, sub, "unshare");
        var c: Cursor = .{ .verb = if (is_unshare) "nvmeof unshare" else "nvmeof adopt", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const nqn = try exactlyOne(&c, "the subsystem NQN");
        return .{ .nvmeof = if (is_unshare) .{ .unshare = .{ .subnqn = nqn } } else .{ .adopt = .{ .subnqn = nqn } } };
    }
    if (std.mem.eql(u8, sub, "list") or std.mem.eql(u8, sub, "status")) {
        const is_list = std.mem.eql(u8, sub, "list");
        var c: Cursor = .{ .verb = if (is_list) "nvmeof list" else "nvmeof status", .words = rest, .diag = diag };
        var json = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
        }
        if (c.pos().len != 0) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = c.pos()[0] } };
            return error.Refused;
        }
        return .{ .nvmeof = if (is_list) .{ .list = .{ .json = json } } else .{ .status = .{ .json = json } } };
    }
    if (std.mem.eql(u8, sub, "restore")) {
        var c: Cursor = .{ .verb = "nvmeof restore", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        if (c.pos().len != 0) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = c.pos()[0] } };
            return error.Refused;
        }
        return .{ .nvmeof = .restore };
    }
    if (std.mem.eql(u8, sub, "target")) {
        const t = try subverb("nvmeof target", rest, diag);
        var c: Cursor = .{ .verb = "nvmeof target", .words = rest[1..], .diag = diag };
        var json = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json") and std.mem.eql(u8, t, "status")) json = true else return c.unknown(f);
        }
        if (c.pos().len != 0) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = c.pos()[0] } };
            return error.Refused;
        }
        if (std.mem.eql(u8, t, "status")) return .{ .nvmeof = .{ .target = .{ .status = .{ .json = json } } } };
        if (std.mem.eql(u8, t, "setup")) return .{ .nvmeof = .{ .target = .setup } };
        if (std.mem.eql(u8, t, "systemd-unit")) return .{ .nvmeof = .{ .target = .systemd_unit } };
        if (std.mem.eql(u8, t, "stop")) return .{ .nvmeof = .{ .target = .stop } };
        diag.* = .{ .unknown_subverb = .{ .verb = "nvmeof target", .word = t } };
        return error.Refused;
    }
    if (std.mem.eql(u8, sub, "connect")) {
        var c: Cursor = .{ .verb = "nvmeof connect", .words = rest, .diag = diag };
        var out: NvmeofConnect = .{ .subnqn = undefined };
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--transport")) {
                out.transport = try parseTransport(&c, f);
            } else if (std.mem.eql(u8, f.name, "--ip")) {
                out.ip = try c.value(f);
            } else if (std.mem.eql(u8, f.name, "--port")) {
                out.port = try parseIntFlag(&c, f, u16, 1, std.math.maxInt(u16));
            } else if (std.mem.eql(u8, f.name, "--hostnqn")) {
                out.hostnqn = try c.value(f);
            } else if (std.mem.eql(u8, f.name, "--hostid")) {
                out.hostid = try c.value(f);
            } else if (std.mem.eql(u8, f.name, "--host-traddr")) {
                out.host_traddr = try c.value(f);
            } else if (std.mem.eql(u8, f.name, "--host-iface")) {
                out.host_iface = try c.value(f);
            } else if (std.mem.eql(u8, f.name, "--nr-io-queues")) {
                // 0 is admitted by the grammar and refused by the verb BEFORE
                // any side effect (the refusal names the minimum).
                out.nr_io_queues = try parseIntFlag(&c, f, u32, 0, 65535);
            } else if (std.mem.eql(u8, f.name, "--ns-uuid")) {
                out.ns_uuid = try parseUuidFlag(&c, f);
            } else return c.unknown(f);
        }
        out.subnqn = try exactlyOne(&c, "the subsystem NQN");
        if ((out.hostnqn == null) != (out.hostid == null)) {
            diag.* = .{ .identity_half = if (out.hostnqn != null) "--hostnqn" else "--hostid" };
            return error.Refused;
        }
        return .{ .nvmeof = .{ .connect = out } };
    }
    if (std.mem.eql(u8, sub, "disconnect")) {
        var c: Cursor = .{ .verb = "nvmeof disconnect", .words = rest, .diag = diag };
        var identity_only = false;
        var hostnqn: ?[]const u8 = null;
        var hostid: ?[]const u8 = null;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--identity-only")) {
                identity_only = true;
            } else if (std.mem.eql(u8, f.name, "--hostnqn")) {
                hostnqn = try c.value(f);
            } else if (std.mem.eql(u8, f.name, "--hostid")) {
                hostid = try c.value(f);
            } else return c.unknown(f);
        }
        const nqn = try exactlyOne(&c, "the subsystem NQN");
        if ((hostnqn == null) != (hostid == null)) {
            diag.* = .{ .identity_half = if (hostnqn != null) "--hostnqn" else "--hostid" };
            return error.Refused;
        }
        return .{ .nvmeof = .{ .disconnect = .{ .subnqn = nqn, .identity_only = identity_only, .hostnqn = hostnqn, .hostid = hostid } } };
    }
    if (std.mem.eql(u8, sub, "resv-report")) {
        var c: Cursor = .{ .verb = "nvmeof resv-report", .words = rest, .diag = diag };
        var json = false;
        var matrix = false;
        var force = false;
        var peer: ?[]const u8 = null;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json")) {
                json = true;
            } else if (std.mem.eql(u8, f.name, "--pr-matrix")) {
                matrix = true;
            } else if (std.mem.eql(u8, f.name, "--force")) {
                force = true;
            } else if (std.mem.eql(u8, f.name, "--peer")) {
                peer = try c.value(f);
            } else return c.unknown(f);
        }
        const dev = try exactlyOne(&c, "the namespace node (/dev/nvmeXnY or /dev/ngXnY)");
        if (matrix and peer == null) {
            diag.* = .matrix_without_peer;
            return error.Refused;
        }
        return .{ .nvmeof = .{ .resv_report = .{ .device = dev, .json = json, .pr_matrix = matrix, .peer = peer, .force = force } } };
    }
    diag.* = .{ .unknown_subverb = .{ .verb = "nvmeof", .word = sub } };
    return error.Refused;
}

fn parseStorage(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("storage", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "list")) {
        var c: Cursor = .{ .verb = "storage list", .words = rest, .diag = diag };
        var json = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
        }
        if (c.pos().len != 0) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = c.pos()[0] } };
            return error.Refused;
        }
        return .{ .storage = .{ .list = .{ .json = json } } };
    }
    if (std.mem.eql(u8, sub, "validate")) {
        var c: Cursor = .{ .verb = "storage validate", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        return .{ .storage = .{ .validate = .{ .path = try exactlyOne(&c, "the device or file path") } } };
    }
    diag.* = .{ .unknown_subverb = .{ .verb = "storage", .word = sub } };
    return error.Refused;
}

fn parseKey(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("key", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "install")) {
        var c: Cursor = .{ .verb = "key install", .words = rest, .diag = diag };
        var force = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--force")) force = true else return c.unknown(f);
        }
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// volume" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the key file (or - for stdin)" } };
            return error.Refused;
        }
        if (pos.len > 2) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
            return error.Refused;
        }
        return .{ .key = .{ .install = .{ .meta_uri = pos[0], .source = pos[1], .force = force } } };
    }
    if (std.mem.eql(u8, sub, "show")) {
        var c: Cursor = .{ .verb = "key show", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const target = try exactlyOne(&c, "the sqmeta:// volume");
        try checkUri(.meta, target, diag);
        return .{ .key = .{ .show = .{ .meta_uri = target } } };
    }
    if (std.mem.eql(u8, sub, "check")) {
        var c: Cursor = .{ .verb = "key check", .words = rest, .diag = diag };
        var flag: ?[]const u8 = null;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--encrypt-key")) flag = try c.value(f) else return c.unknown(f);
        }
        const target = try exactlyOne(&c, "the sqmeta:// volume");
        try checkUri(.meta, target, diag);
        return .{ .key = .{ .check = .{ .meta_uri = target, .encrypt_key = flag } } };
    }
    if (std.mem.eql(u8, sub, "tls-init")) {
        var c: Cursor = .{ .verb = "key tls-init", .words = rest, .diag = diag };
        var name: ?[]const u8 = null;
        var force = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--name")) name = try c.value(f) else if (std.mem.eql(u8, f.name, "--force")) force = true else return c.unknown(f);
        }
        const dir = try exactlyOne(&c, "the directory for ca.pem and ca.key");
        return .{ .key = .{ .tls_init = .{ .dir = dir, .name = name, .force = force } } };
    }
    if (std.mem.eql(u8, sub, "tls-issue")) {
        var c: Cursor = .{ .verb = "key tls-issue", .words = rest, .diag = diag };
        var node: ?[]const u8 = null;
        var out: ?[]const u8 = null;
        var force = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--node")) node = try c.value(f) else if (std.mem.eql(u8, f.name, "--out")) out = try c.value(f) else if (std.mem.eql(u8, f.name, "--force")) force = true else return c.unknown(f);
        }
        const dir = try exactlyOne(&c, "the CA directory (ca.pem + ca.key)");
        const n = node orelse {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "--node NAME (the certificate's CN)" } };
            return error.Refused;
        };
        return .{ .key = .{ .tls_issue = .{ .dir = dir, .node = n, .out = out, .force = force } } };
    }
    diag.* = .{ .unknown_subverb = .{ .verb = "key", .word = sub } };
    return error.Refused;
}

/// One path of an endpoint spec: `<tcp|loop>:<traddr>:<trsvcid>` (the
/// traddr and trsvcid empty on loop: `loop::`).
pub const EndpointPath = struct { transport: Transport, traddr: []const u8, trsvcid: u16 };

/// `vol-<16 hex>=<path>[,<path>…]:<subnqn>` — one subsystem reached over
/// one or more paths (one per target listener), every path naming the
/// SAME subnqn: `vol-…=tcp:10.0.0.1:4420:nqn.x,tcp:10.0.1.1:4420:nqn.x`,
/// `vol-…=loop:::nqn.x`.
pub const EndpointSpec = struct {
    vol_tag: u64,
    subnqn: []const u8,
    paths: [MAX_REPEAT]EndpointPath,
    n: usize,

    pub fn slice(self: *const EndpointSpec) []const EndpointPath {
        return self.paths[0..self.n];
    }
};

/// Why a spec is refused, for the refusal text: the shape itself, two
/// paths naming different subsystems (both named), or one path twice.
pub const EndpointSpecRefusal = union(enum) {
    shape,
    subnqn_differs: struct { a: []const u8, b: []const u8 },
    repeated_path: []const u8,
    too_many_paths,
};

/// One `<tcp|loop>:<traddr>:<trsvcid>:<subnqn>` word; null on a shape
/// outside the grammar.
fn parseEndpointPath(word: []const u8, out: *EndpointPath) ?[]const u8 {
    var it = std.mem.splitScalar(u8, word, ':');
    const tr_text = it.next() orelse return null;
    const transport: Transport = if (std.mem.eql(u8, tr_text, "tcp")) .tcp else if (std.mem.eql(u8, tr_text, "loop")) .loop else return null;
    const traddr = it.next() orelse return null;
    const svc_text = it.next() orelse return null;
    const subnqn = it.rest();
    if (subnqn.len == 0 or !std.mem.startsWith(u8, subnqn, "nqn.")) return null;
    const trsvcid: u16 = if (svc_text.len == 0) 0 else std.fmt.parseInt(u16, svc_text, 10) catch return null;
    if (transport == .tcp and (traddr.len == 0 or trsvcid == 0)) return null;
    if (transport == .loop and (traddr.len != 0 or svc_text.len != 0)) return null;
    out.* = .{ .transport = transport, .traddr = traddr, .trsvcid = trsvcid };
    return subnqn;
}

pub fn parseEndpointSpecDiag(word: []const u8, why: *EndpointSpecRefusal) ?EndpointSpec {
    why.* = .shape;
    const eq = std.mem.indexOfScalar(u8, word, '=') orelse return null;
    const tag_text = word[0..eq];
    if (!std.mem.startsWith(u8, tag_text, "vol-") or tag_text.len != 20) return null;
    var spec: EndpointSpec = .{ .vol_tag = std.fmt.parseInt(u64, tag_text[4..], 16) catch return null, .subnqn = "", .paths = undefined, .n = 0 }; // vol-<16 hex>
    var it = std.mem.splitScalar(u8, word[eq + 1 ..], ',');
    while (it.next()) |path_word| {
        if (spec.n == MAX_REPEAT) {
            why.* = .too_many_paths;
            return null;
        }
        const nqn = parseEndpointPath(path_word, &spec.paths[spec.n]) orelse return null;
        if (spec.n == 0) {
            spec.subnqn = nqn;
        } else if (!std.mem.eql(u8, nqn, spec.subnqn)) {
            why.* = .{ .subnqn_differs = .{ .a = spec.subnqn, .b = nqn } };
            return null;
        }
        const p = spec.paths[spec.n];
        for (spec.paths[0..spec.n]) |q| {
            if (q.transport == p.transport and q.trsvcid == p.trsvcid and std.mem.eql(u8, q.traddr, p.traddr)) {
                why.* = .{ .repeated_path = path_word };
                return null;
            }
        }
        spec.n += 1;
    }
    return spec;
}

pub fn parseEndpointSpec(word: []const u8) ?EndpointSpec {
    var why: EndpointSpecRefusal = undefined;
    return parseEndpointSpecDiag(word, &why);
}

/// A set-level target: a `sqmeta://` URI (validated by the ONE parser) or
/// an absolute mountpoint (resolved through `.config` by the verb).
pub fn checkTarget(word: []const u8, diag: *Refusal) Error!void {
    if (uri.schemeOf(word)) |scheme| {
        if (scheme != .meta) {
            diag.* = .{ .bad_target = word };
            return error.Refused;
        }
        return checkUri(.meta, word, diag);
    }
    if (word.len == 0 or word[0] != '/') {
        diag.* = .{ .bad_target = word };
        return error.Refused;
    }
}

/// Is the target a mountpoint (vs a URI)?
pub fn targetIsMountpoint(word: []const u8) bool {
    return uri.schemeOf(word) == null;
}

fn exactlyOne(c: *Cursor, what: []const u8) Error![]const u8 {
    const pos = c.pos();
    if (pos.len < 1) {
        c.diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = what } };
        return error.Refused;
    }
    if (pos.len > 1) {
        c.diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[1] } };
        return error.Refused;
    }
    return pos[0];
}

const target_what = "the sqmeta:// volume or the mountpoint";

/// `clients` / `df`: `[--json] TARGET`.
fn parseTargetVerb(comptime tag: std.meta.Tag(Verb), words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = @tagName(tag), .words = words, .diag = diag };
    var json = false;
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
    }
    const target = try exactlyOne(&c, target_what);
    try checkTarget(target, diag);
    return switch (tag) {
        .clients => .{ .clients = .{ .target = target, .json = json } },
        .df => .{ .df = .{ .target = target, .json = json } },
        else => comptime unreachable,
    };
}

fn checkSetPath(word: []const u8, diag: *Refusal) Error!void {
    if (word.len < 2 or word[0] != '/' or word[word.len - 1] == '/') {
        diag.* = .{ .bad_set_path = word };
        return error.Refused;
    }
}

fn parseClone(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "clone", .words = words, .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const pos = c.pos();
    if (pos.len < 1) {
        diag.* = .{ .missing_positional = .{ .verb = "clone", .what = target_what } };
        return error.Refused;
    }
    try checkTarget(pos[0], diag);
    if (pos.len < 2) {
        diag.* = .{ .missing_positional = .{ .verb = "clone", .what = "the source path" } };
        return error.Refused;
    }
    if (pos.len < 3) {
        diag.* = .{ .missing_positional = .{ .verb = "clone", .what = "the destination path" } };
        return error.Refused;
    }
    if (pos.len > 3) {
        diag.* = .{ .extra_positional = .{ .verb = "clone", .word = pos[3] } };
        return error.Refused;
    }
    try checkSetPath(pos[1], diag);
    try checkSetPath(pos[2], diag);
    return .{ .clone = .{ .target = pos[0], .src = pos[1], .dest = pos[2] } };
}

fn parseTune(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "tune", .words = words, .diag = diag };
    var out: Tune = .{};
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--apply")) out.apply = true else return c.unknown(f);
    }
    if (c.pos().len != 0) {
        diag.* = .{ .extra_positional = .{ .verb = "tune", .word = c.pos()[0] } };
        return error.Refused;
    }
    return .{ .tune = out };
}

fn subverb(verb: []const u8, words: []const []const u8, diag: *Refusal) Error![]const u8 {
    if (words.len == 0) {
        diag.* = .{ .missing_subverb = verb };
        return error.Refused;
    }
    return words[0];
}

fn parseConfig(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("config", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "show")) {
        var c: Cursor = .{ .verb = "config show", .words = rest, .diag = diag };
        var json = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
        }
        const target = try exactlyOne(&c, target_what);
        try checkTarget(target, diag);
        return .{ .config = .{ .show = .{ .target = target, .json = json } } };
    }
    if (std.mem.eql(u8, sub, "set-fabric-endpoints")) {
        var c: Cursor = .{ .verb = "config set-fabric-endpoints", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// volume" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "at least one vol-<tag>=<trtype>:<traddr>:<trsvcid>:<subnqn>" } };
            return error.Refused;
        }
        var specs: RepeatList = .{};
        for (pos[1..]) |word| {
            var why: EndpointSpecRefusal = undefined;
            if (parseEndpointSpecDiag(word, &why) == null) {
                diag.* = .{ .bad_endpoint_spec = .{ .word = word, .why = why } };
                return error.Refused;
            }
            if (specs.n == MAX_REPEAT) {
                diag.* = .{ .too_many_repeats = "endpoint specs" };
                return error.Refused;
            }
            specs.items[specs.n] = word;
            specs.n += 1;
        }
        return .{ .config = .{ .set_fabric_endpoints = .{ .target = pos[0], .specs = specs } } };
    }
    if (std.mem.eql(u8, sub, "set-dir-spread-depth")) {
        var c: Cursor = .{ .verb = "config set-dir-spread-depth", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the depth (0 … 254)" } };
            return error.Refused;
        }
        const depth = switch (knobs.parseIntIn(u8, "depth", pos[0], 0, dir_place.DEPTH_MAX)) {
            .value => |v| v,
            .absent, .invalid => {
                diag.* = .{ .bad_dir_spread_depth = pos[0] };
                return error.Refused;
            },
        };
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// volume" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[1], diag);
        if (pos.len > 2) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
            return error.Refused;
        }
        return .{ .config = .{ .set_dir_spread_depth = .{ .target = pos[1], .depth = depth } } };
    }
    if (std.mem.eql(u8, sub, "get-fabric-endpoints")) {
        var c: Cursor = .{ .verb = "config get-fabric-endpoints", .words = rest, .diag = diag };
        var json = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
        }
        const target = try exactlyOne(&c, target_what);
        try checkTarget(target, diag);
        return .{ .config = .{ .get_fabric_endpoints = .{ .target = target, .json = json } } };
    }
    diag.* = .{ .unknown_subverb = .{ .verb = "config", .word = sub } };
    return error.Refused;
}

fn parseClaim(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("claim", words, diag);
    if (!std.mem.eql(u8, sub, "clear")) {
        diag.* = .{ .unknown_subverb = .{ .verb = "claim", .word = sub } };
        return error.Refused;
    }
    var c: Cursor = .{ .verb = "claim clear", .words = words[1..], .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const target = try exactlyOne(&c, "the sqmeta:// volume");
    try checkUri(.meta, target, diag);
    return .{ .claim_clear = .{ .meta_uri = target } };
}

fn parseAppenders(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "appenders", .words = words, .diag = diag };
    var json = false;
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--json")) {
            json = true;
        } else return c.unknown(f);
    }
    const target = try exactlyOne(&c, "the sqmeta:// volume");
    try checkUri(.meta, target, diag);
    return .{ .appenders = .{ .meta_uri = target, .json = json } };
}

fn parseAppenderClear(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("appender", words, diag);
    if (!std.mem.eql(u8, sub, "clear")) {
        diag.* = .{ .unknown_subverb = .{ .verb = "appender", .word = sub } };
        return error.Refused;
    }
    var c: Cursor = .{ .verb = "appender clear", .words = words[1..], .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const pos = c.pos();
    if (pos.len < 2) {
        diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = if (pos.len == 0) "the sqmeta:// volume" else "the appender id" } };
        return error.Refused;
    }
    if (pos.len > 2) {
        diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
        return error.Refused;
    }
    try checkUri(.meta, pos[0], diag);
    // The id through the ONE integer parser (the verb's word as its name).
    const id: u32 = switch (knobs.parseIntIn(u32, "appender id", pos[1], 0, std.math.maxInt(u32))) {
        .value => |v| v,
        .absent => {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the appender id" } };
            return error.Refused;
        },
        .invalid => |d| {
            diag.* = .{ .bad_int = d };
            return error.Refused;
        },
    };
    return .{ .appender_clear = .{ .meta_uri = pos[0], .appender_id = id } };
}

/// @anchor:ops_fsck_parse — `fsck` / `scrub` / `fsck merge-reports`.
fn parseFsck(words: []const []const u8, diag: *Refusal, scrub_only: bool) Error!Verb {
    if (!scrub_only and words.len >= 1 and std.mem.eql(u8, words[0], "merge-reports")) {
        // Flags first, then the files: the list is the argv tail itself (a
        // `Verb` borrows argv and owns no heap).
        var json = false;
        var i: usize = 1;
        while (i < words.len and words[i].len >= 2 and words[i][0] == '-') : (i += 1) {
            if (std.mem.eql(u8, words[i], "--json")) {
                json = true;
            } else {
                diag.* = .{ .unknown_flag = .{ .verb = "fsck merge-reports", .flag = words[i] } };
                return error.Refused;
            }
        }
        if (i == words.len) {
            diag.* = .{ .missing_positional = .{ .verb = "fsck merge-reports", .what = "at least one report file" } };
            return error.Refused;
        }
        return .{ .fsck_merge = .{ .files = words[i..], .json = json } };
    }
    var c: Cursor = .{ .verb = if (scrub_only) "scrub" else "fsck", .words = words, .diag = diag };
    var out: Fsck = .{ .target = "", .scrub_only = scrub_only };
    var shards_raw: ?[]const u8 = null;
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--online")) {
            out.mode = .online;
        } else if (std.mem.eql(u8, f.name, "--offline")) {
            out.mode = .offline;
        } else if (std.mem.eql(u8, f.name, "--json")) {
            out.json = true;
        } else if (std.mem.eql(u8, f.name, "--throttle")) {
            out.throttle = try parseIntFlag(&c, f, u8, 0, 100);
        } else if (!scrub_only and std.mem.eql(u8, f.name, "--scrub")) {
            out.scrub = true;
        } else if (!scrub_only and std.mem.eql(u8, f.name, "--shards")) {
            shards_raw = try c.value(f);
        } else if (!scrub_only and std.mem.eql(u8, f.name, "--repair")) {
            out.repair = true;
        } else if (!scrub_only and std.mem.eql(u8, f.name, "--apply")) {
            out.apply = true;
        } else if (!scrub_only and std.mem.eql(u8, f.name, "--quarantine-dir")) {
            out.quarantine_dir = try c.value(f);
        } else return c.unknown(f);
    }
    out.target = try exactlyOne(&c, target_what);
    try checkTarget(out.target, diag);
    if (shards_raw) |raw| {
        out.shards = parseShards(raw) orelse {
            diag.* = .{ .bad_shards = raw };
            return error.Refused;
        };
    }
    // The lattice (`ops_fsck.md` §6): every refusal names the fix.
    const mountpoint = targetIsMountpoint(out.target);
    if (out.mode == .offline and mountpoint) return lattice(diag, "--offline needs the sqmeta:// URI (a mountpoint names a live daemon)");
    if (out.mode == .online and !mountpoint) return lattice(diag, "--online needs the mountpoint (a sqmeta:// URI is the offline set)");
    if ((out.apply or out.quarantine_dir != null) and !out.repair) return lattice(diag, "--apply / --quarantine-dir need --repair");
    if (out.shards != null and out.repair) return lattice(diag, "--repair is refused on a shard (merge the shard reports, then repair whole)");
    if (out.quarantine_dir) |d| if (d.len == 0 or d[0] != '/') return lattice(diag, "--quarantine-dir must be an absolute path");
    return .{ .fsck = out };
}

fn lattice(diag: *Refusal, why: []const u8) Error {
    diag.* = .{ .fsck_lattice = why };
    return error.Refused;
}

/// `k/N`, `0 ≤ k < N`, `N ≥ 1` (the ONE partition key's spelling).
pub fn parseShards(raw: []const u8) ?FsckShard {
    const slash = std.mem.indexOfScalar(u8, raw, '/') orelse return null;
    const k = std.fmt.parseInt(u32, raw[0..slash], 10) catch return null;
    const n = std.fmt.parseInt(u32, raw[slash + 1 ..], 10) catch return null;
    if (n == 0 or k >= n) return null;
    return .{ .k = k, .n = n };
}

fn parseVolume(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("volume", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "list") or std.mem.eql(u8, sub, "repair-set") or std.mem.eql(u8, sub, "enable-symmetric")) {
        const is_list = std.mem.eql(u8, sub, "list");
        const is_enable = std.mem.eql(u8, sub, "enable-symmetric");
        var c: Cursor = .{ .verb = if (is_list) "volume list" else if (is_enable) "volume enable-symmetric" else "volume repair-set", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const target = try exactlyOne(&c, "the sqmeta:// set");
        try checkUri(.meta, target, diag);
        if (is_list) return .{ .volume = .{ .list = .{ .meta_uri = target } } };
        if (is_enable) return .{ .volume = .{ .enable_symmetric = .{ .meta_uri = target } } };
        return .{ .volume = .{ .repair_set = .{ .meta_uri = target } } };
    }
    if (std.mem.eql(u8, sub, "add-meta")) {
        var c: Cursor = .{ .verb = "volume add-meta", .words = rest, .diag = diag };
        var take: ?TakeSlots = null;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--take-slots")) {
                const raw = try c.value(f);
                take = parseTakeSlots(raw) orelse {
                    diag.* = .{ .bad_take_slots = raw };
                    return error.Refused;
                };
            } else return c.unknown(f);
        }
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// set" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the device to add (an absolute path)" } };
            return error.Refused;
        }
        if (pos.len > 2) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
            return error.Refused;
        }
        if (pos[1].len == 0 or pos[1][0] != '/') {
            diag.* = .{ .bad_target = pos[1] };
            return error.Refused;
        }
        return .{ .volume = .{ .add_meta = .{ .meta_uri = pos[0], .device = pos[1], .take = take orelse {
            diag.* = .{ .missing_value = .{ .verb = c.verb, .flag = "--take-slots" } };
            return error.Refused;
        } } } };
    }
    if (std.mem.eql(u8, sub, "remove-meta")) {
        var c: Cursor = .{ .verb = "volume remove-meta", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// set" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the victim device (an absolute path)" } };
            return error.Refused;
        }
        if (pos.len > 2) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
            return error.Refused;
        }
        if (pos[1].len == 0 or pos[1][0] != '/') {
            diag.* = .{ .bad_target = pos[1] };
            return error.Refused;
        }
        return .{ .volume = .{ .remove_meta = .{ .meta_uri = pos[0], .victim = pos[1] } } };
    }
    if (std.mem.eql(u8, sub, "migrate-meta-slot")) {
        var c: Cursor = .{ .verb = "volume migrate-meta-slot", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const pos = c.pos();
        if (pos.len < 1) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the sqmeta:// set" } };
            return error.Refused;
        }
        try checkUri(.meta, pos[0], diag);
        if (pos.len < 2) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the slot (0..65535)" } };
            return error.Refused;
        }
        if (pos.len < 3) {
            diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = "the target member index" } };
            return error.Refused;
        }
        if (pos.len > 3) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[3] } };
            return error.Refused;
        }
        const slot = try parseIntWord(diag, "slot", pos[1], 0, std.math.maxInt(u16));
        const target = try parseIntWord(diag, "target member index", pos[2], 0, uri.MAX_MEMBERS - 1);
        return .{ .volume = .{ .migrate_meta_slot = .{ .meta_uri = pos[0], .slot = @intCast(slot), .target = @intCast(target) } } };
    }
    // @anchor:ops_jobs_volume_parse
    if (std.mem.eql(u8, sub, "add-data")) {
        var c: Cursor = .{ .verb = "volume add-data", .words = rest, .diag = diag };
        var no_rebalance = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--no-rebalance")) no_rebalance = true else return c.unknown(f);
        }
        // The target: the sqmeta:// set (offline) or the mountpoint (the
        // daemon's admin lane).
        const pos = try exactlyTwo(&c, "the sqmeta:// set or the mountpoint", "the data device");
        try checkTarget(pos[0], diag);
        return .{ .volume = .{ .add_data = .{ .meta_uri = pos[0], .device = pos[1], .no_rebalance = no_rebalance } } };
    }
    if (std.mem.eql(u8, sub, "remove-data") or std.mem.eql(u8, sub, "undrain")) {
        const is_remove = std.mem.eql(u8, sub, "remove-data");
        var c: Cursor = .{ .verb = if (is_remove) "volume remove-data" else "volume undrain", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const pos = try exactlyTwo(&c, "the sqmeta:// set or the mountpoint", "the volume id (vol-<16 hex>)");
        try checkTarget(pos[0], diag);
        if (!isVolumeIdWord(pos[1])) {
            diag.* = .{ .bad_volume_id = pos[1] };
            return error.Refused;
        }
        return .{ .volume = if (is_remove) .{ .remove_data = .{ .meta_uri = pos[0], .volume = pos[1] } } else .{ .undrain = .{ .meta_uri = pos[0], .volume = pos[1] } } };
    }
    diag.* = .{ .unknown_subverb = .{ .verb = "volume", .word = sub } };
    return error.Refused;
}

/// `vol-` + 16 hex digits.
pub fn isVolumeIdWord(w: []const u8) bool {
    if (w.len != 20 or !std.mem.startsWith(u8, w, "vol-")) return false;
    for (w[4..]) |c| _ = std.fmt.charToDigit(c, 16) catch return false;
    return true;
}

/// `job-` + 16 hex digits, never 0.
pub fn parseJobIdWord(w: []const u8) ?u64 {
    if (w.len != 20 or !std.mem.startsWith(u8, w, "job-")) return null;
    const v = std.fmt.parseInt(u64, w[4..], 16) catch return null;
    return if (v == 0) null else v;
}

fn exactlyTwo(c: *Cursor, first: []const u8, second: []const u8) Error![2][]const u8 {
    const pos = c.pos();
    if (pos.len < 1) {
        c.diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = first } };
        return error.Refused;
    }
    if (pos.len < 2) {
        c.diag.* = .{ .missing_positional = .{ .verb = c.verb, .what = second } };
        return error.Refused;
    }
    if (pos.len > 2) {
        c.diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = pos[2] } };
        return error.Refused;
    }
    return .{ pos[0], pos[1] };
}

/// @anchor:ops_jobs_parse — `job *`.
fn parseJob(words: []const []const u8, diag: *Refusal) Error!Verb {
    const sub = try subverb("job", words, diag);
    const rest = words[1..];
    if (std.mem.eql(u8, sub, "list")) {
        var c: Cursor = .{ .verb = "job list", .words = rest, .diag = diag };
        var json = false;
        while (try c.step()) |f| {
            if (std.mem.eql(u8, f.name, "--json")) json = true else return c.unknown(f);
        }
        const target = try exactlyOne(&c, target_what);
        try checkTarget(target, diag);
        return .{ .job = .{ .list = .{ .target = target, .json = json } } };
    }
    if (std.mem.eql(u8, sub, "worker") or std.mem.eql(u8, sub, "rotate-enroll")) {
        const is_worker = std.mem.eql(u8, sub, "worker");
        var c: Cursor = .{ .verb = if (is_worker) "job worker" else "job rotate-enroll", .words = rest, .diag = diag };
        while (try c.step()) |f| return c.unknown(f);
        const target = try exactlyOne(&c, "the sqmeta:// set");
        try checkUri(.meta, target, diag);
        return .{ .job = if (is_worker) .{ .worker = .{ .meta_uri = target } } else .{ .rotate_enroll = .{ .meta_uri = target } } };
    }
    const Kind = enum { status, pause, @"resume", cancel, throttle };
    const kind = std.meta.stringToEnum(Kind, sub) orelse {
        diag.* = .{ .unknown_subverb = .{ .verb = "job", .word = sub } };
        return error.Refused;
    };
    var c: Cursor = .{ .verb = "job", .words = rest, .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const pos = c.pos();
    if (pos.len < 1) {
        diag.* = .{ .missing_positional = .{ .verb = "job", .what = target_what } };
        return error.Refused;
    }
    if (pos.len < 2) {
        diag.* = .{ .missing_positional = .{ .verb = "job", .what = "the job id (job-<16 hex>)" } };
        return error.Refused;
    }
    try checkTarget(pos[0], diag);
    const id = parseJobIdWord(pos[1]) orelse {
        diag.* = .{ .bad_job_id = pos[1] };
        return error.Refused;
    };
    switch (kind) {
        .throttle => {
            if (pos.len < 3) {
                diag.* = .{ .missing_positional = .{ .verb = "job throttle", .what = "the duty pct (1..100)" } };
                return error.Refused;
            }
            if (pos.len > 3) {
                diag.* = .{ .extra_positional = .{ .verb = "job throttle", .word = pos[3] } };
                return error.Refused;
            }
            const pct = try parseIntWord(diag, "pct", pos[2], 1, 100);
            return .{ .job = .{ .throttle = .{ .target = pos[0], .job_id = id, .pct = @intCast(pct) } } };
        },
        else => {
            if (pos.len > 2) {
                diag.* = .{ .extra_positional = .{ .verb = "job", .word = pos[2] } };
                return error.Refused;
            }
            return .{ .job = switch (kind) {
                .status => .{ .status = .{ .target = pos[0], .job_id = id } },
                .pause => .{ .pause = .{ .target = pos[0], .job_id = id } },
                .@"resume" => .{ .@"resume" = .{ .target = pos[0], .job_id = id } },
                .cancel => .{ .cancel = .{ .target = pos[0], .job_id = id } },
                .throttle => unreachable,
            } };
        },
    }
}

/// `defrag` — the arms are flags; `--volume` names a `vol-<16 hex>`.
fn parseDefrag(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "defrag", .words = words, .diag = diag };
    var d: Defrag = .{ .target = "" };
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--data")) d.data = true else if (std.mem.eql(u8, f.name, "--pack")) d.pack = true else if (std.mem.eql(u8, f.name, "--meta")) d.meta = true else if (std.mem.eql(u8, f.name, "--fold")) d.fold = true else if (std.mem.eql(u8, f.name, "--rebalance")) d.rebalance = true else if (std.mem.eql(u8, f.name, "--report-only")) d.report_only = true else if (std.mem.eql(u8, f.name, "--rekey")) d.rekey = true else if (std.mem.eql(u8, f.name, "--json")) d.json = true else if (std.mem.eql(u8, f.name, "--throttle")) {
            d.throttle = try parseIntFlag(&c, f, u8, 1, 100);
        } else if (std.mem.eql(u8, f.name, "--encrypt-key")) {
            d.encrypt_key = try c.value(f);
        } else if (std.mem.eql(u8, f.name, "--volume")) {
            const raw = try c.value(f);
            if (!isVolumeIdWord(raw)) {
                diag.* = .{ .bad_volume_id = raw };
                return error.Refused;
            }
            d.volume = std.fmt.parseInt(u64, raw[4..], 16) catch unreachable;
        } else return c.unknown(f);
    }
    d.target = try exactlyOne(&c, target_what);
    try checkTarget(d.target, diag);
    if (!d.anyArm()) {
        diag.* = .{ .missing_positional = .{ .verb = "defrag", .what = "an arm: --data, --pack, --meta, --fold, --rebalance, --rekey or --report-only" } };
        return error.Refused;
    }
    if (d.rekey) {
        if (d.encrypt_key == null) {
            diag.* = .{ .rekey_shape = .no_key };
            return error.Refused;
        }
        if (d.data or d.pack or d.meta or d.fold or d.rebalance or d.report_only or d.volume != null) {
            diag.* = .{ .rekey_shape = .other_arm };
            return error.Refused;
        }
        if (!targetIsMountpoint(d.target)) {
            diag.* = .{ .rekey_shape = .offline };
            return error.Refused;
        }
    } else if (d.encrypt_key != null) {
        diag.* = .{ .rekey_shape = .key_without_rekey };
        return error.Refused;
    }
    return .{ .defrag = d };
}

/// A positional integer through the ONE parser.
fn parseIntWord(diag: *Refusal, what: []const u8, word: []const u8, lo: u32, hi: u32) Error!u32 {
    return switch (knobs.parseIntIn(u32, what, word, lo, hi)) {
        .value => |v| v,
        .absent => blk: {
            diag.* = .{ .missing_positional = .{ .verb = "volume", .what = what } };
            break :blk error.Refused;
        },
        .invalid => |d| blk: {
            diag.* = .{ .bad_int = d };
            break :blk error.Refused;
        },
    };
}

/// `K` (≥ 1) or `s1,s2,…` (each a slot id < 65536; a list of one is a list).
pub fn parseTakeSlots(raw: []const u8) ?TakeSlots {
    if (raw.len == 0) return null;
    if (std.mem.indexOfScalar(u8, raw, ',') == null) {
        if (std.fmt.parseInt(u32, raw, 10)) |k| return if (k >= 1) .{ .count = k } else null else |_| return null;
    }
    var it = std.mem.splitScalar(u8, raw, ',');
    while (it.next()) |m| _ = std.fmt.parseInt(u16, m, 10) catch return null;
    return .{ .list = raw };
}

/// `<integer>[k|m|g|t]` (powers of 1024), ≥ 1.
pub fn parseSize(word: []const u8) ?u64 {
    if (word.len == 0) return null;
    var digits = word;
    var shift: u6 = 0;
    switch (std.ascii.toLower(word[word.len - 1])) {
        'k' => shift = 10,
        'm' => shift = 20,
        'g' => shift = 30,
        't' => shift = 40,
        '0'...'9' => {},
        else => return null,
    }
    if (shift != 0) digits = word[0 .. word.len - 1];
    const n = std.fmt.parseInt(u64, digits, 10) catch return null;
    if (n == 0) return null;
    if (shift != 0 and n > (@as(u64, std.math.maxInt(u64)) >> shift)) return null;
    return n << shift;
}

fn parseSizeFlag(c: *Cursor, f: Flag) Error!u64 {
    const raw = try c.value(f);
    return parseSize(raw) orelse {
        c.diag.* = .{ .bad_size = .{ .flag = f.name, .word = raw } };
        return error.Refused;
    };
}

fn parseBench(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "bench", .words = words, .diag = diag };
    var out: Bench = .{ .mountpoint = undefined };
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "-w") or std.mem.eql(u8, f.name, "--write")) {
            out.write = true;
        } else if (std.mem.eql(u8, f.name, "-r") or std.mem.eql(u8, f.name, "--read")) {
            out.read = true;
        } else if (std.mem.eql(u8, f.name, "--stat")) {
            out.stat = true;
        } else if (std.mem.eql(u8, f.name, "--del")) {
            out.del = true;
        } else if (std.mem.eql(u8, f.name, "-t") or std.mem.eql(u8, f.name, "--threads")) {
            out.threads = try parseIntFlag(&c, f, u32, 1, 4096);
        } else if (std.mem.eql(u8, f.name, "-n") or std.mem.eql(u8, f.name, "--files")) {
            out.files = try parseIntFlag(&c, f, u32, 1, 1 << 20);
        } else if (std.mem.eql(u8, f.name, "-s") or std.mem.eql(u8, f.name, "--size")) {
            out.size = try parseSizeFlag(&c, f);
        } else if (std.mem.eql(u8, f.name, "-b") or std.mem.eql(u8, f.name, "--block")) {
            out.block = try parseSizeFlag(&c, f);
        } else if (std.mem.eql(u8, f.name, "--rand")) {
            out.rand = true;
        } else if (std.mem.eql(u8, f.name, "--direct")) {
            out.direct = true;
        } else if (std.mem.eql(u8, f.name, "--time")) {
            out.time = try parseIntFlag(&c, f, u32, 0, 86_400);
        } else if (std.mem.eql(u8, f.name, "-i") or std.mem.eql(u8, f.name, "--iterations")) {
            out.iterations = try parseIntFlag(&c, f, u32, 1, 1 << 20);
        } else if (std.mem.eql(u8, f.name, "--smoke")) {
            out.smoke = true;
        } else if (std.mem.eql(u8, f.name, "--abba")) {
            out.abba = true;
        } else if (std.mem.eql(u8, f.name, "--json")) {
            out.json = true;
        } else if (std.mem.eql(u8, f.name, "--hash")) {
            out.hash = true;
        } else return c.unknown(f);
    }
    if (out.hash) {
        // CPU only: no mountpoint, and no shape word may pretend otherwise.
        if (c.pos().len != 0) {
            diag.* = .{ .extra_positional = .{ .verb = c.verb, .word = c.pos()[0] } };
            return error.Refused;
        }
        if (out.anyPhase() or out.smoke or out.abba or out.block != null or out.rand) {
            diag.* = .{ .bench_hash_with_shape = {} };
            return error.Refused;
        }
        out.mountpoint = "";
        return .{ .bench = out };
    }
    out.mountpoint = try exactlyOne(&c, "the mountpoint");
    if (out.mountpoint.len == 0 or out.mountpoint[0] != '/') {
        diag.* = .{ .bad_target = out.mountpoint };
        return error.Refused;
    }
    // The bare invocation runs the fixed suite: a block or an access
    // pattern without a phase would silently reshape it.
    if (!out.anyPhase() and !out.smoke) {
        if (out.block != null) {
            diag.* = .{ .bench_suite_with_shape = "-b" };
            return error.Refused;
        }
        if (out.rand) {
            diag.* = .{ .bench_suite_with_shape = "--rand" };
            return error.Refused;
        }
    }
    return .{ .bench = out };
}

fn parseFormat(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "format", .words = words, .diag = diag };
    var out: Format = .{ .meta_uri = undefined, .data_uris = &.{} };
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--single-writer")) {
            out.single_writer = true;
        } else if (std.mem.eql(u8, f.name, "--force")) {
            out.force = true;
        } else if (std.mem.eql(u8, f.name, "--meta-node-kib")) {
            const kib = try parseIntFlag(&c, f, u32, NODE_KIB_MIN, NODE_KIB_MAX);
            var warn_buf: [256]u8 = undefined;
            const nk = meta_geometry.validateNodeKib(kib, &warn_buf) catch {
                diag.* = .{ .bad_node_kib = kib };
                return error.Refused;
            };
            out.node_size = nk.bytes;
        } else if (std.mem.eql(u8, f.name, "--meta-journal-mb")) {
            out.journal_len = @as(u64, try parseIntFlag(&c, f, u32, JOURNAL_MB_MIN, JOURNAL_MB_MAX)) << 20;
        } else if (std.mem.eql(u8, f.name, "--dir-spread-depth")) {
            out.dir_spread_depth = try parseIntFlag(&c, f, u8, 0, dir_place.DEPTH_MAX);
        } else if (std.mem.eql(u8, f.name, "--block-size")) {
            const b = try parseIntFlag(&c, f, u64, transform.MIN_BLOCK_SIZE, transform.MAX_BLOCK_SIZE);
            if (!transform.blockSizeValid(b)) {
                diag.* = .{ .bad_block_size = b };
                return error.Refused;
            }
            out.block_size = @intCast(b);
        } else if (std.mem.eql(u8, f.name, "--compress")) {
            const v = try c.value(f);
            out.compression = transform.Compression.parse(v) orelse {
                diag.* = .{ .bad_compression = v };
                return error.Refused;
            };
        } else if (std.mem.eql(u8, f.name, "--encrypt")) {
            const v = try c.value(f);
            out.encryption = transform.Encryption.parse(v) orelse {
                diag.* = .{ .bad_encryption = v };
                return error.Refused;
            };
        } else if (std.mem.eql(u8, f.name, "--encrypt-key")) {
            out.encrypt_key = try c.value(f);
        } else return c.unknown(f);
    }
    if (out.encryption != .none and out.encrypt_key == null) {
        diag.* = .encrypt_without_key;
        return error.Refused;
    }
    if (out.encryption == .none and out.encrypt_key != null) {
        diag.* = .key_without_encrypt;
        return error.Refused;
    }
    // Positionals: exactly one sqmeta://; the sqdata:// words are the TAIL
    // of argv (one spelling: `… sqmeta://m sqdata://a sqdata://b`), so the
    // verb can borrow them as one slice of the caller's argv.
    var meta: ?[]const u8 = null;
    for (c.pos()) |w| {
        switch (uri.schemeOf(w) orelse {
            diag.* = .{ .bad_uri = .{ .wrong_scheme = .{ .want = .meta, .got = w } } };
            return error.Refused;
        }) {
            .meta => {
                if (meta != null) {
                    diag.* = .{ .extra_positional = .{ .verb = "format", .word = w } };
                    return error.Refused;
                }
                try checkUri(.meta, w, diag);
                meta = w;
            },
            .data => {
                if (meta == null) {
                    diag.* = .{ .missing_positional = .{ .verb = "format", .what = "the sqmeta:// volume (it precedes every sqdata:// volume)" } };
                    return error.Refused;
                }
            },
        }
    }
    out.meta_uri = meta orelse {
        diag.* = .{ .missing_positional = .{ .verb = "format", .what = "the sqmeta:// volume" } };
        return error.Refused;
    };
    out.data_uris = try dataTail(words, diag);
    return .{ .format = out };
}

/// The `sqdata://` words as the tail of argv: from the first one on, every
/// word must be a valid data URI (a flag or another positional after it is
/// refused by name).
fn dataTail(words: []const []const u8, diag: *Refusal) Error![]const []const u8 {
    const first = for (words, 0..) |w, i| {
        if (uri.schemeOf(w) == .data) break i;
    } else return &.{};
    for (words[first..]) |w| {
        if (uri.schemeOf(w) != .data) {
            diag.* = .{ .extra_positional = .{ .verb = "format", .word = w } };
            return error.Refused;
        }
        try checkUri(.data, w, diag);
    }
    return words[first..];
}

fn parseMount(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "mount", .words = words, .diag = diag };
    var out: Mount = .{ .meta_uri = undefined, .mountpoint = undefined };
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "-o")) {
            out.options = try c.value(f);
        } else if (std.mem.eql(u8, f.name, "--daemon")) {
            out.daemon = true;
        } else if (std.mem.eql(u8, f.name, "--supervise")) {
            out.supervise = true;
        } else if (std.mem.eql(u8, f.name, "--log-file")) {
            out.log_file = try c.value(f);
        } else if (std.mem.eql(u8, f.name, "--join")) {
            out.join = try c.value(f);
        } else if (std.mem.eql(u8, f.name, "--encrypt-key")) {
            out.encrypt_key = try c.value(f);
        } else return c.unknown(f);
    }
    const pos = c.pos();
    if (pos.len < 1) {
        diag.* = .{ .missing_positional = .{ .verb = "mount", .what = "the sqmeta:// volume" } };
        return error.Refused;
    }
    try checkUri(.meta, pos[0], diag);
    if (pos.len < 2) {
        diag.* = .{ .missing_positional = .{ .verb = "mount", .what = "the mountpoint" } };
        return error.Refused;
    }
    if (pos.len > 2) {
        diag.* = .{ .extra_positional = .{ .verb = "mount", .word = pos[2] } };
        return error.Refused;
    }
    out.meta_uri = pos[0];
    out.mountpoint = pos[1];
    if (out.supervise and !out.daemon) {
        diag.* = .supervise_without_daemon;
        return error.Refused;
    }
    if (out.log_file != null and !out.daemon) {
        diag.* = .log_file_without_daemon;
        return error.Refused;
    }
    if (out.daemon and out.log_file == null) {
        diag.* = .daemon_without_log_file;
        return error.Refused;
    }
    return .{ .mount = out };
}

fn parseUmount(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "umount", .words = words, .diag = diag };
    var lazy = false;
    while (try c.step()) |f| {
        if (std.mem.eql(u8, f.name, "--lazy") or std.mem.eql(u8, f.name, "-l")) lazy = true else return c.unknown(f);
    }
    const pos = c.pos();
    if (pos.len < 1) {
        diag.* = .{ .missing_positional = .{ .verb = "umount", .what = "the mountpoint" } };
        return error.Refused;
    }
    if (pos.len > 1) {
        diag.* = .{ .extra_positional = .{ .verb = "umount", .word = pos[1] } };
        return error.Refused;
    }
    return .{ .umount = .{ .mountpoint = pos[0], .lazy = lazy } };
}

fn parseStatus(words: []const []const u8, diag: *Refusal) Error!Verb {
    var c: Cursor = .{ .verb = "status", .words = words, .diag = diag };
    while (try c.step()) |f| return c.unknown(f);
    const target = try exactlyOne(&c, target_what);
    try checkTarget(target, diag);
    return .{ .status = .{ .target = target } };
}

// ---------------------------------------------------------------------------
// Tests: the grammar's contract.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn av(comptime words: []const []const u8) []const []const u8 {
    return &([_][]const u8{"squeezefs"} ++ words.*);
}

fn refusal(words: []const []const u8) !Refusal {
    var d: Refusal = undefined;
    try testing.expectError(error.Refused, parse(words, &d));
    return d;
}

fn text(buf: []u8, r: Refusal) []const u8 {
    return std.fmt.bufPrint(buf, "{f}", .{r}) catch unreachable;
}

test "version and help" {
    var d: Refusal = undefined;
    try testing.expectEqual(Verb.version, try parse(av(&.{"--version"}), &d));
    try testing.expectEqual(Verb.version, try parse(av(&.{"version"}), &d));
    try testing.expectEqual(Verb.help, try parse(av(&.{"--help"}), &d));
    try testing.expectEqual(Verb.help, try parse(av(&.{"-h"}), &d));
    try testing.expect((try refusal(av(&.{}))) == .no_verb);
    try testing.expect((try refusal(av(&.{"frobnicate"}))) == .unknown_verb);
}

test "format: flags in any order, --k=v and --k v, --meta-node-kib validated against the five, journal MiB → bytes, data URIs are the argv tail" {
    var d: Refusal = undefined;
    const v = try parse(av(&.{ "format", "--force", "--meta-node-kib=64", "--meta-journal-mb", "4", "--single-writer", "sqmeta:///build/tmp/m.img", "sqdata:///dev/a", "sqdata:///dev/b,/dev/c" }), &d);
    try testing.expectEqualStrings("sqmeta:///build/tmp/m.img", v.format.meta_uri);
    try testing.expect(v.format.force and v.format.single_writer);
    try testing.expectEqual(@as(?u64, 65536), v.format.node_size);
    try testing.expectEqual(@as(?u64, 4 << 20), v.format.journal_len);
    try testing.expectEqual(@as(usize, 2), v.format.data_uris.len);
    try testing.expectEqualStrings("sqdata:///dev/b,/dev/c", v.format.data_uris[1]);
    const plain = try parse(av(&.{ "format", "sqmeta:///dev/nvme0n1" }), &d);
    try testing.expect(!plain.format.force and plain.format.node_size == null and plain.format.journal_len == null and plain.format.data_uris.len == 0);
    try testing.expectEqual(dir_place.DEFAULT_DEPTH, plain.format.dir_spread_depth);
    // `--` ends the flags.
    const dashed = try parse(av(&.{ "format", "--", "sqmeta:///x" }), &d);
    try testing.expectEqualStrings("sqmeta:///x", dashed.format.meta_uri);
}

test "the directory spread depth (decision 0003 §BZ): `format --dir-spread-depth` 0, 10 and the ceiling 254 parse, 255 and junk refuse; `config set-dir-spread-depth N sqmeta://…` parses, refuses a bad depth, a missing URI and an extra word" {
    var d: Refusal = undefined;
    var buf: [512]u8 = undefined;
    inline for (.{ "0", "10", "254" }, [_]u8{ 0, 10, dir_place.DEPTH_MAX }) |word, want| {
        const v = try parse(av(&.{ "format", "--dir-spread-depth", word, "sqmeta:///x" }), &d);
        try testing.expectEqual(want, v.format.dir_spread_depth);
    }
    const eq = try parse(av(&.{ "format", "--dir-spread-depth=10", "sqmeta:///x" }), &d);
    try testing.expectEqual(@as(u8, 10), eq.format.dir_spread_depth);
    var r = try refusal(av(&.{ "format", "--dir-spread-depth", "255", "sqmeta:///x" }));
    try testing.expect(r == .bad_int);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "0..=254") != null);
    r = try refusal(av(&.{ "format", "--dir-spread-depth", "deep", "sqmeta:///x" }));
    try testing.expect(r == .bad_int);
    const c = try parse(av(&.{ "config", "set-dir-spread-depth", "10", "sqmeta:///build/tmp/m.img" }), &d);
    try testing.expectEqual(@as(u8, 10), c.config.set_dir_spread_depth.depth);
    try testing.expectEqualStrings("sqmeta:///build/tmp/m.img", c.config.set_dir_spread_depth.target);
    const zero = try parse(av(&.{ "config", "set-dir-spread-depth", "0", "sqmeta:///m" }), &d);
    try testing.expectEqual(@as(u8, 0), zero.config.set_dir_spread_depth.depth);
    r = try refusal(av(&.{ "config", "set-dir-spread-depth", "255", "sqmeta:///m" }));
    try testing.expect(r == .bad_dir_spread_depth);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "'255' is not a depth in 0 … 254") != null);
    r = try refusal(av(&.{ "config", "set-dir-spread-depth", "three", "sqmeta:///m" }));
    try testing.expect(r == .bad_dir_spread_depth);
    r = try refusal(av(&.{ "config", "set-dir-spread-depth", "3" }));
    try testing.expect(r == .missing_positional);
    r = try refusal(av(&.{ "config", "set-dir-spread-depth" }));
    try testing.expect(r == .missing_positional);
    r = try refusal(av(&.{ "config", "set-dir-spread-depth", "3", "sqmeta:///m", "extra" }));
    try testing.expect(r == .extra_positional);
    r = try refusal(av(&.{ "config", "set-dir-spread-depth", "3", "/mnt/x" }));
    try testing.expect(r == .bad_uri);
    try testing.expect(std.mem.indexOf(u8, usage(.config), "set-dir-spread-depth 0…254") != null);
    try testing.expect(std.mem.indexOf(u8, usage(.format), "--dir-spread-depth 0…254") != null);
}

test "format refusals: a sixth node size, an out-of-range journal, junk integers, two metadata URIs, a relative path, a data URI before the metadata one or followed by a flag, no URI" {
    var buf: [512]u8 = undefined;
    var r = try refusal(av(&.{ "format", "--meta-node-kib", "96", "sqmeta:///x" }));
    try testing.expect(r == .bad_node_kib);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "64|128|256|512|1024") != null);
    r = try refusal(av(&.{ "format", "--meta-node-kib", "32", "sqmeta:///x" }));
    try testing.expect(r == .bad_int);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "64..=1024") != null);
    r = try refusal(av(&.{ "format", "--meta-journal-mb=0", "sqmeta:///x" }));
    try testing.expect(r == .bad_int);
    r = try refusal(av(&.{ "format", "--meta-journal-mb", "4MiB", "sqmeta:///x" }));
    try testing.expect(r == .bad_int);
    r = try refusal(av(&.{ "format", "--meta-journal-mb" }));
    try testing.expect(r == .missing_value);
    r = try refusal(av(&.{ "format", "sqmeta:///x", "sqmeta:///y" }));
    try testing.expect(r == .extra_positional);
    r = try refusal(av(&.{ "format", "sqmeta://x" }));
    try testing.expect(r == .bad_uri);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "must be absolute") != null);
    r = try refusal(av(&.{ "format", "sqdata:///d", "sqmeta:///x" }));
    try testing.expect(r == .missing_positional);
    r = try refusal(av(&.{ "format", "sqmeta:///x", "sqdata:///d", "--force" }));
    try testing.expect(r == .extra_positional);
    r = try refusal(av(&.{ "format", "sqmeta:///x", "sqdata://d" }));
    try testing.expect(r == .bad_uri);
    r = try refusal(av(&.{ "format", "--force" }));
    try testing.expect(r == .missing_positional);
    r = try refusal(av(&.{ "format", "/dev/nvme0n1" }));
    try testing.expect(r == .bad_uri);
    r = try refusal(av(&.{ "format", "--full", "sqmeta:///x" }));
    try testing.expect(r == .unknown_flag);
}

test "format: the data-plane flags — block size (power of two in the window), compress, encrypt + key (both or neither); a cache-path flag is unknown (the write cache needs none)" {
    var d: Refusal = undefined;
    var buf: [512]u8 = undefined;
    const v = try parse(av(&.{ "format", "--block-size", "1048576", "--compress=zstd", "--encrypt", "chacha20-poly1305", "--encrypt-key", "/etc/squeezefs/keys/x.key", "sqmeta:///m", "sqdata:///d" }), &d);
    try testing.expectEqual(@as(u32, 1 << 20), v.format.block_size);
    try testing.expectEqual(transform.Compression.zstd, v.format.compression);
    try testing.expectEqual(transform.Encryption.chacha20poly1305, v.format.encryption);
    try testing.expectEqualStrings("/etc/squeezefs/keys/x.key", v.format.encrypt_key.?);
    const plain = try parse(av(&.{ "format", "sqmeta:///m" }), &d);
    try testing.expectEqual(transform.DEFAULT_BLOCK_SIZE, plain.format.block_size);
    try testing.expect(plain.format.compression == .none and plain.format.encryption == .none and plain.format.encrypt_key == null);
    const stdin_key = try parse(av(&.{ "format", "--encrypt", "aes-256-gcm", "--encrypt-key", "-", "sqmeta:///m" }), &d);
    try testing.expectEqualStrings("-", stdin_key.format.encrypt_key.?);
    var r = try refusal(av(&.{ "format", "--block-size", "3145728", "sqmeta:///m" }));
    try testing.expect(r == .bad_block_size);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "power of two") != null);
    r = try refusal(av(&.{ "format", "--block-size", "2048", "sqmeta:///m" }));
    try testing.expect(r == .bad_int);
    r = try refusal(av(&.{ "format", "--compress", "gzip", "sqmeta:///m" }));
    try testing.expect(r == .bad_compression);
    r = try refusal(av(&.{ "format", "--encrypt", "aes256gcm", "sqmeta:///m" }));
    try testing.expect(r == .bad_encryption);
    r = try refusal(av(&.{ "format", "--encrypt", "aes-256-gcm", "sqmeta:///m" }));
    try testing.expect(r == .encrypt_without_key);
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "--encrypt-key") != null);
    r = try refusal(av(&.{ "format", "--encrypt-key", "/k", "sqmeta:///m" }));
    try testing.expect(r == .key_without_encrypt);
    r = try refusal(av(&.{ "format", "--disk-cache-paths", "/build/c", "sqmeta:///m" }));
    try testing.expect(r == .unknown_flag);
}

test "mount: -o glued or spaced, --daemon/--supervise/--log-file, two positionals in order" {
    var d: Refusal = undefined;
    const v = try parse(av(&.{ "mount", "-oallow_other,attr_timeout=2", "--daemon", "--supervise", "--log-file=/var/log/sqz.log", "sqmeta:///dev/m", "/mnt/sqz" }), &d);
    try testing.expectEqualStrings("allow_other,attr_timeout=2", v.mount.options.?);
    try testing.expect(v.mount.daemon and v.mount.supervise);
    try testing.expectEqualStrings("/var/log/sqz.log", v.mount.log_file.?);
    try testing.expectEqualStrings("sqmeta:///dev/m", v.mount.meta_uri);
    try testing.expectEqualStrings("/mnt/sqz", v.mount.mountpoint);
    const fg = try parse(av(&.{ "mount", "sqmeta:///dev/m", "/mnt/sqz", "-o", "ro" }), &d);
    try testing.expectEqualStrings("ro", fg.mount.options.?);
    try testing.expect(!fg.mount.daemon and fg.mount.log_file == null);
}

test "mount refusals: supervise/log-file without daemon, daemon without a log file, a missing mountpoint, a third positional, a bad URI, an unknown flag" {
    var r = try refusal(av(&.{ "mount", "--supervise", "sqmeta:///m", "/mnt" }));
    try testing.expect(r == .supervise_without_daemon);
    r = try refusal(av(&.{ "mount", "--log-file", "/l", "sqmeta:///m", "/mnt" }));
    try testing.expect(r == .log_file_without_daemon);
    r = try refusal(av(&.{ "mount", "--daemon", "sqmeta:///m", "/mnt" }));
    try testing.expect(r == .daemon_without_log_file);
    r = try refusal(av(&.{ "mount", "sqmeta:///m" }));
    try testing.expect(r == .missing_positional);
    r = try refusal(av(&.{ "mount", "sqmeta:///m", "/mnt", "extra" }));
    try testing.expect(r == .extra_positional);
    r = try refusal(av(&.{ "mount", "/dev/m", "/mnt" }));
    try testing.expect(r == .bad_uri);
    r = try refusal(av(&.{ "mount", "--fg", "sqmeta:///m", "/mnt" }));
    try testing.expect(r == .unknown_flag);
    r = try refusal(av(&.{ "mount", "-o" }));
    try testing.expect(r == .missing_value);
}

test "umount and status take exactly one positional; umount's --lazy/-l is the explicit opt-in to the lazy detach" {
    var d: Refusal = undefined;
    try testing.expectEqualStrings("/mnt/x", (try parse(av(&.{ "umount", "/mnt/x" }), &d)).umount.mountpoint);
    try testing.expect(!(try parse(av(&.{ "umount", "/mnt/x" }), &d)).umount.lazy);
    try testing.expect((try parse(av(&.{ "umount", "--lazy", "/mnt/x" }), &d)).umount.lazy);
    try testing.expect((try parse(av(&.{ "umount", "-l", "/mnt/x" }), &d)).umount.lazy);
    try testing.expectEqualStrings("/mnt/x", (try parse(av(&.{ "umount", "/mnt/x", "-l" }), &d)).umount.mountpoint);
    try testing.expectEqualStrings("sqmeta:///v", (try parse(av(&.{ "status", "sqmeta:///v" }), &d)).status.target);
    try testing.expectEqualStrings("/mnt/sqz", (try parse(av(&.{ "status", "/mnt/sqz" }), &d)).status.target);
    try testing.expect((try refusal(av(&.{ "status", "sqdata:///d" }))) == .bad_target);
    try testing.expect((try refusal(av(&.{ "status", "relative" }))) == .bad_target);
    try testing.expect((try refusal(av(&.{"umount"}))) == .missing_positional);
    try testing.expect((try refusal(av(&.{ "umount", "/a", "/b" }))) == .extra_positional);
    try testing.expect((try refusal(av(&.{ "umount", "--force", "/a" }))) == .unknown_flag);
    try testing.expect((try refusal(av(&.{"status"}))) == .missing_positional);
}

test "rung-5 verbs: clients/df/config show take [--json] TARGET (URI or mountpoint); clone TARGET SRC DEST with absolute set paths; tune [--apply]; claim clear takes a URI only; a cache-path verb and the staging verb are unknown; sub-verbs refuse by name" {
    var d: Refusal = undefined;
    const cl = try parse(av(&.{ "clients", "--json", "/mnt/sqz" }), &d);
    try testing.expect(cl.clients.json and std.mem.eql(u8, cl.clients.target, "/mnt/sqz"));
    const df = try parse(av(&.{ "df", "sqmeta:///dev/m" }), &d);
    try testing.expect(!df.df.json and targetIsMountpoint("/mnt") and !targetIsMountpoint(df.df.target));
    const cn = try parse(av(&.{ "clone", "sqmeta:///dev/m", "/a/b", "/a/c" }), &d);
    try testing.expectEqualStrings("/a/b", cn.clone.src);
    try testing.expectEqualStrings("/a/c", cn.clone.dest);
    try testing.expect((try refusal(av(&.{ "clone", "sqmeta:///dev/m", "a/b", "/a/c" }))) == .bad_set_path);
    try testing.expect((try refusal(av(&.{ "clone", "sqmeta:///dev/m", "/a/b", "/a/c/" }))) == .bad_set_path);
    try testing.expect((try refusal(av(&.{ "clone", "sqmeta:///dev/m", "/a/b" }))) == .missing_positional);
    try testing.expect((try refusal(av(&.{ "clone", "sqmeta:///dev/m", "/a/b", "/a/c", "/x" }))) == .extra_positional);
    try testing.expect((try parse(av(&.{ "tune", "--apply" }), &d)).tune.apply);
    try testing.expect(!(try parse(av(&.{"tune"}), &d)).tune.apply);
    try testing.expect((try refusal(av(&.{ "tune", "x" }))) == .extra_positional);
    const cs = try parse(av(&.{ "config", "show", "--json", "sqmeta:///dev/m" }), &d);
    try testing.expect(cs.config.show.json);
    try testing.expect((try refusal(av(&.{ "config", "set-cache-paths", "/mnt/sqz", "/build/c1" }))) == .unknown_subverb);
    try testing.expect((try refusal(av(&.{"config"}))) == .missing_subverb);
    try testing.expect((try refusal(av(&.{ "config", "list" }))) == .unknown_subverb);
    try testing.expect((try refusal(av(&.{ "staging", "list", "/mnt/sqz" }))) == .unknown_verb);
    const cc = try parse(av(&.{ "claim", "clear", "sqmeta:///dev/m,/dev/n" }), &d);
    try testing.expectEqualStrings("sqmeta:///dev/m,/dev/n", cc.claim_clear.meta_uri);
    try testing.expect((try refusal(av(&.{ "claim", "clear", "/mnt" }))) == .bad_uri);
    try testing.expect((try refusal(av(&.{ "claim", "show", "sqmeta:///dev/m" }))) == .unknown_subverb);
}

test "defrag: the arms; --rekey needs --encrypt-key, runs alone and on a mountpoint; --encrypt-key belongs to --rekey" {
    var d: Refusal = undefined;
    const a = try parse(av(&.{ "defrag", "--data", "--pack", "--throttle", "25", "/mnt/sqz" }), &d);
    try testing.expect(a.defrag.data and a.defrag.pack and !a.defrag.rekey and a.defrag.throttle == 25);
    const r = try parse(av(&.{ "defrag", "--rekey", "--encrypt-key", "/etc/new.key", "--throttle", "50", "/mnt/sqz" }), &d);
    try testing.expect(r.defrag.rekey and r.defrag.throttle == 50);
    try testing.expectEqualStrings("/etc/new.key", r.defrag.encrypt_key.?);
    const stdin_key = try parse(av(&.{ "defrag", "--rekey", "--encrypt-key", "-", "/mnt/sqz" }), &d);
    try testing.expectEqualStrings("-", stdin_key.defrag.encrypt_key.?);
    try testing.expectEqual(Refusal{ .rekey_shape = .no_key }, try refusal(av(&.{ "defrag", "--rekey", "/mnt/sqz" })));
    try testing.expectEqual(Refusal{ .rekey_shape = .key_without_rekey }, try refusal(av(&.{ "defrag", "--data", "--encrypt-key", "/k", "/mnt/sqz" })));
    try testing.expectEqual(Refusal{ .rekey_shape = .other_arm }, try refusal(av(&.{ "defrag", "--rekey", "--encrypt-key", "/k", "--pack", "/mnt/sqz" })));
    try testing.expectEqual(Refusal{ .rekey_shape = .offline }, try refusal(av(&.{ "defrag", "--rekey", "--encrypt-key", "/k", "sqmeta:///dev/m" })));
    try testing.expect((try refusal(av(&.{ "defrag", "/mnt/sqz" }))) == .missing_positional);
}

test "bench: every flag; sizes accept k/m/g/t and refuse garbage and 0; the bare invocation refuses -b and --rand; iterations ≥ 1; the mountpoint is absolute" {
    var d: Refusal = undefined;
    const b = try parse(av(&.{ "bench", "-w", "-r", "--stat", "--del", "-t", "4", "-n", "2", "-s", "64m", "-b", "4k", "--rand", "--direct", "--time", "30", "-i", "3", "--json", "/mnt/sqz" }), &d);
    try testing.expect(b.bench.write and b.bench.read and b.bench.stat and b.bench.del and b.bench.rand and b.bench.direct and b.bench.json);
    try testing.expectEqual(@as(?u32, 4), b.bench.threads);
    try testing.expectEqual(@as(?u32, 2), b.bench.files);
    try testing.expectEqual(@as(?u64, 64 << 20), b.bench.size);
    try testing.expectEqual(@as(?u64, 4096), b.bench.block);
    try testing.expectEqual(@as(?u32, 30), b.bench.time);
    try testing.expectEqual(@as(u32, 3), b.bench.iterations);
    const bare = try parse(av(&.{ "bench", "/mnt/sqz" }), &d);
    try testing.expect(!bare.bench.anyPhase() and bare.bench.iterations == 1 and bare.bench.threads == null);
    try testing.expect((try parse(av(&.{ "bench", "--smoke", "/mnt/sqz" }), &d)).bench.smoke);
    try testing.expect((try parse(av(&.{ "bench", "-r", "--abba", "/mnt/sqz" }), &d)).bench.abba);
    try testing.expectEqual(@as(?u64, 1), parseSize("1"));
    try testing.expectEqual(@as(?u64, 1 << 10), parseSize("1k"));
    try testing.expectEqual(@as(?u64, 3 << 20), parseSize("3M"));
    try testing.expectEqual(@as(?u64, 2 << 30), parseSize("2g"));
    try testing.expectEqual(@as(?u64, 1 << 40), parseSize("1T"));
    try testing.expect(parseSize("") == null and parseSize("0") == null and parseSize("4x") == null and parseSize("k") == null and parseSize("-1") == null and parseSize("99999999999999999999g") == null);
    try testing.expect((try refusal(av(&.{ "bench", "-s", "junk", "/mnt/sqz" }))) == .bad_size);
    try testing.expect((try refusal(av(&.{ "bench", "-b", "4k", "/mnt/sqz" }))) == .bench_suite_with_shape);
    try testing.expect((try refusal(av(&.{ "bench", "--rand", "/mnt/sqz" }))) == .bench_suite_with_shape);
    try testing.expect((try refusal(av(&.{ "bench", "-i", "0", "/mnt/sqz" }))) == .bad_int);
    try testing.expect((try refusal(av(&.{ "bench", "mnt" }))) == .bad_target);
    try testing.expect((try refusal(av(&.{"bench"}))) == .missing_positional);
    // The CPU hash microbench: no mountpoint; `--json` is its only companion.
    const hb = try parse(av(&.{ "bench", "--hash", "--json" }), &d);
    try testing.expect(hb.bench.hash and hb.bench.json and hb.bench.mountpoint.len == 0);
    try testing.expect((try refusal(av(&.{ "bench", "--hash", "/mnt/sqz" }))) == .extra_positional);
    try testing.expect((try refusal(av(&.{ "bench", "--hash", "-w" }))) == .bench_hash_with_shape);
    try testing.expect((try refusal(av(&.{ "bench", "--hash", "--smoke" }))) == .bench_hash_with_shape);
}

test "appenders [--json] TARGET and appender clear TARGET ID: the URI checked, the id through the one integer parser, sub-verbs refused by name" {
    var d: Refusal = undefined;
    const a = try parse(av(&.{ "appenders", "--json", "sqmeta:///dev/m" }), &d);
    try testing.expect(a.appenders.json);
    try testing.expectEqualStrings("sqmeta:///dev/m", a.appenders.meta_uri);
    try testing.expect((try refusal(av(&.{ "appenders", "/mnt" }))) == .bad_uri);
    const c = try parse(av(&.{ "appender", "clear", "sqmeta:///dev/m", "3" }), &d);
    try testing.expectEqual(@as(u32, 3), c.appender_clear.appender_id);
    try testing.expect((try refusal(av(&.{ "appender", "clear", "sqmeta:///dev/m" }))) == .missing_positional);
    try testing.expect((try refusal(av(&.{ "appender", "clear", "sqmeta:///dev/m", "x" }))) == .bad_int);
    try testing.expect((try refusal(av(&.{ "appender", "clear", "sqmeta:///dev/m", "1", "2" }))) == .extra_positional);
    try testing.expect((try refusal(av(&.{ "appender", "show", "sqmeta:///dev/m", "1" }))) == .unknown_subverb);
}

test "nvmeof: share collects repeatable --ip/--allow-host, parses --transport/--port/--create-size/--ns-uuid; connect pairs hostnqn/hostid; disconnect --identity-only; resv-report --pr-matrix needs --peer; target sub-verbs; storage and key grammars; mount --encrypt-key; config fabric-endpoint specs" {
    var d: Refusal = undefined;
    const s = try parse(av(&.{ "nvmeof", "share", "--transport", "loop", "--ip", "127.0.0.1", "--ip", "::1", "--port", "4421", "--allow-host", "nqn.h1", "--create-size", "1g", "--ns-uuid", "11111111-2222-3333-4444-555555555555", "--subnqn", "nqn.x", "/build/a.img" }), &d);
    try testing.expectEqual(Transport.loop, s.nvmeof.share.transport);
    try testing.expectEqual(@as(usize, 2), s.nvmeof.share.ips.n);
    try testing.expectEqualStrings("::1", s.nvmeof.share.ips.slice()[1]);
    try testing.expectEqual(@as(u16, 4421), s.nvmeof.share.port);
    try testing.expectEqualStrings("nqn.h1", s.nvmeof.share.allow_hosts.slice()[0]);
    try testing.expectEqual(@as(?u64, 1 << 30), s.nvmeof.share.create_size);
    try testing.expectEqual(@as(u8, 0x11), s.nvmeof.share.ns_uuid.?[0]);
    try testing.expectEqualStrings("nqn.x", s.nvmeof.share.subnqn.?);
    try testing.expectEqualStrings("/build/a.img", s.nvmeof.share.backing);
    try testing.expect((try refusal(av(&.{ "nvmeof", "share", "--transport", "rdma", "/x" }))) == .bad_transport);
    try testing.expect((try refusal(av(&.{ "nvmeof", "share", "--ns-uuid", "zz", "/x" }))) == .bad_uuid);
    try testing.expect((try refusal(av(&.{ "nvmeof", "share", "--port", "0", "/x" }))) == .bad_int);
    try testing.expect((try refusal(av(&.{ "nvmeof", "share" }))) == .missing_positional);
    const u = try parse(av(&.{ "nvmeof", "unshare", "nqn.x" }), &d);
    try testing.expectEqualStrings("nqn.x", u.nvmeof.unshare.subnqn);
    const l = try parse(av(&.{ "nvmeof", "list", "--json" }), &d);
    try testing.expect(l.nvmeof.list.json);
    try testing.expect((try parse(av(&.{ "nvmeof", "status", "--json" }), &d)).nvmeof.status.json);
    try testing.expect(!(try parse(av(&.{ "nvmeof", "status" }), &d)).nvmeof.status.json);
    try testing.expect((try refusal(av(&.{ "nvmeof", "status", "extra" }))) == .extra_positional);
    try testing.expect((try parse(av(&.{ "nvmeof", "restore" }), &d)).nvmeof == .restore);
    try testing.expectEqualStrings("nqn.y", (try parse(av(&.{ "nvmeof", "adopt", "nqn.y" }), &d)).nvmeof.adopt.subnqn);
    try testing.expect((try parse(av(&.{ "nvmeof", "target", "status", "--json" }), &d)).nvmeof.target.status.json);
    try testing.expect((try parse(av(&.{ "nvmeof", "target", "setup" }), &d)).nvmeof.target == .setup);
    try testing.expect((try parse(av(&.{ "nvmeof", "target", "systemd-unit" }), &d)).nvmeof.target == .systemd_unit);
    try testing.expect((try parse(av(&.{ "nvmeof", "target", "stop" }), &d)).nvmeof.target == .stop);
    try testing.expect((try refusal(av(&.{ "nvmeof", "target", "install" }))) == .unknown_subverb);
    const c = try parse(av(&.{ "nvmeof", "connect", "--ip", "127.0.0.1", "--port", "4420", "--hostnqn", "nqn.h", "--hostid", "id", "--nr-io-queues", "0", "nqn.x" }), &d);
    try testing.expectEqualStrings("127.0.0.1", c.nvmeof.connect.ip.?);
    try testing.expectEqual(@as(?u32, 0), c.nvmeof.connect.nr_io_queues);
    try testing.expect((try refusal(av(&.{ "nvmeof", "connect", "--hostnqn", "nqn.h", "nqn.x" }))) == .identity_half);
    try testing.expect((try refusal(av(&.{ "nvmeof", "connect", "--hostid", "id", "nqn.x" }))) == .identity_half);
    const dc = try parse(av(&.{ "nvmeof", "disconnect", "--identity-only", "nqn.x" }), &d);
    try testing.expect(dc.nvmeof.disconnect.identity_only);
    const rr = try parse(av(&.{ "nvmeof", "resv-report", "--json", "--pr-matrix", "--peer", "/dev/nvme1n1", "--force", "/dev/nvme0n1" }), &d);
    try testing.expect(rr.nvmeof.resv_report.pr_matrix and rr.nvmeof.resv_report.force and rr.nvmeof.resv_report.json);
    try testing.expectEqualStrings("/dev/nvme1n1", rr.nvmeof.resv_report.peer.?);
    try testing.expect((try refusal(av(&.{ "nvmeof", "resv-report", "--pr-matrix", "/dev/nvme0n1" }))) == .matrix_without_peer);
    try testing.expect((try refusal(av(&.{ "nvmeof", "bogus" }))) == .unknown_subverb);
    try testing.expect((try refusal(av(&.{"nvmeof"}))) == .missing_subverb);
    try testing.expect((try parse(av(&.{ "storage", "list", "--json" }), &d)).storage.list.json);
    try testing.expectEqualStrings("/dev/x", (try parse(av(&.{ "storage", "validate", "/dev/x" }), &d)).storage.validate.path);
    try testing.expect((try refusal(av(&.{ "storage", "pool", "create" }))) == .unknown_subverb);
    const ki = try parse(av(&.{ "key", "install", "--force", "sqmeta:///dev/m", "-" }), &d);
    try testing.expect(ki.key.install.force);
    try testing.expectEqualStrings("-", ki.key.install.source);
    try testing.expect((try refusal(av(&.{ "key", "install", "sqmeta:///dev/m" }))) == .missing_positional);
    try testing.expectEqualStrings("sqmeta:///dev/m", (try parse(av(&.{ "key", "show", "sqmeta:///dev/m" }), &d)).key.show.meta_uri);
    const kc = try parse(av(&.{ "key", "check", "--encrypt-key", "/k", "sqmeta:///dev/m" }), &d);
    try testing.expectEqualStrings("/k", kc.key.check.encrypt_key.?);
    try testing.expect((try refusal(av(&.{ "key", "rotate", "sqmeta:///dev/m" }))) == .unknown_subverb);
    const ti = try parse(av(&.{ "key", "tls-init", "--name", "lab-ca", "/etc/squeezefs/tls" }), &d);
    try testing.expectEqualStrings("lab-ca", ti.key.tls_init.name.?);
    try testing.expectEqualStrings("/etc/squeezefs/tls", ti.key.tls_init.dir);
    try testing.expect(!ti.key.tls_init.force);
    const tis = try parse(av(&.{ "key", "tls-issue", "--node", "node-a", "--out", "/tmp/a", "--force", "/etc/squeezefs/tls" }), &d);
    try testing.expectEqualStrings("node-a", tis.key.tls_issue.node);
    try testing.expectEqualStrings("/tmp/a", tis.key.tls_issue.out.?);
    try testing.expect(tis.key.tls_issue.force);
    try testing.expect((try refusal(av(&.{ "key", "tls-issue", "/etc/squeezefs/tls" }))) == .missing_positional);
    try testing.expect((try refusal(av(&.{ "key", "tls-init" }))) == .missing_positional);
    const m = try parse(av(&.{ "mount", "--encrypt-key", "-", "sqmeta:///dev/m", "/mnt" }), &d);
    try testing.expectEqualStrings("-", m.mount.encrypt_key.?);
    const fe = try parse(av(&.{ "config", "set-fabric-endpoints", "sqmeta:///dev/m", "vol-0123456789abcdef=tcp:127.0.0.1:4420:nqn.x", "vol-0000000000000001=loop:::nqn.y" }), &d);
    try testing.expectEqual(@as(usize, 2), fe.config.set_fabric_endpoints.specs.n);
    const spec = parseEndpointSpec("vol-0123456789abcdef=tcp:127.0.0.1:4420:nqn.x").?;
    try testing.expectEqual(@as(u64, 0x0123456789abcdef), spec.vol_tag);
    try testing.expectEqual(@as(usize, 1), spec.n);
    try testing.expectEqual(@as(u16, 4420), spec.slice()[0].trsvcid);
    try testing.expectEqualStrings("127.0.0.1", spec.slice()[0].traddr);
    try testing.expectEqualStrings("nqn.x", spec.subnqn);
    try testing.expect(parseEndpointSpec("vol-01=tcp:1:2:nqn.x") == null);
    try testing.expect(parseEndpointSpec("vol-0123456789abcdef=tcp::4420:nqn.x") == null);
    try testing.expect(parseEndpointSpec("vol-0123456789abcdef=loop:1::nqn.x") == null);
    try testing.expect((try refusal(av(&.{ "config", "set-fabric-endpoints", "sqmeta:///dev/m", "junk" }))) == .bad_endpoint_spec);
    try testing.expect((try parse(av(&.{ "config", "get-fabric-endpoints", "--json", "/mnt" }), &d)).config.get_fabric_endpoints.json);
}

test "endpoint specs with several paths: two tcp paths of one subsystem parse in order; a path naming another subsystem refuses naming both; a repeated path refuses naming it; a 17th path refuses; a trailing comma is a shape refusal; loop and tcp paths mix" {
    const two = parseEndpointSpec("vol-0123456789abcdef=tcp:10.0.0.1:4420:nqn.x,tcp:10.0.1.1:4421:nqn.x").?;
    try testing.expectEqual(@as(usize, 2), two.n);
    try testing.expectEqualStrings("10.0.0.1", two.slice()[0].traddr);
    try testing.expectEqualStrings("10.0.1.1", two.slice()[1].traddr);
    try testing.expectEqual(@as(u16, 4421), two.slice()[1].trsvcid);
    try testing.expectEqualStrings("nqn.x", two.subnqn);
    var why: EndpointSpecRefusal = undefined;
    try testing.expect(parseEndpointSpecDiag("vol-0123456789abcdef=tcp:10.0.0.1:4420:nqn.x,tcp:10.0.1.1:4420:nqn.y", &why) == null);
    try testing.expectEqualStrings("nqn.x", why.subnqn_differs.a);
    try testing.expectEqualStrings("nqn.y", why.subnqn_differs.b);
    try testing.expect(parseEndpointSpecDiag("vol-0123456789abcdef=tcp:10.0.0.1:4420:nqn.x,tcp:10.0.0.1:4420:nqn.x", &why) == null);
    try testing.expectEqualStrings("tcp:10.0.0.1:4420:nqn.x", why.repeated_path);
    try testing.expect(parseEndpointSpecDiag("vol-0123456789abcdef=tcp:10.0.0.1:4420:nqn.x,", &why) == null);
    try testing.expect(why == .shape);
    var many: std.Io.Writer.Allocating = .init(testing.allocator);
    defer many.deinit();
    try many.writer.writeAll("vol-0123456789abcdef=");
    for (0..MAX_REPEAT + 1) |i| try many.writer.print("{s}tcp:10.0.0.{d}:4420:nqn.x", .{ if (i == 0) "" else ",", i });
    try testing.expect(parseEndpointSpecDiag(many.written(), &why) == null);
    try testing.expect(why == .too_many_paths);
    const mixed = parseEndpointSpec("vol-0123456789abcdef=loop:::nqn.x,tcp:10.0.0.1:4420:nqn.x").?;
    try testing.expectEqual(@as(usize, 2), mixed.n);
    try testing.expectEqual(Transport.loop, mixed.slice()[0].transport);
    var d: Refusal = undefined;
    const fe = try parse(av(&.{ "config", "set-fabric-endpoints", "sqmeta:///dev/m", "vol-0123456789abcdef=tcp:10.0.0.1:4420:nqn.x,tcp:10.0.1.1:4420:nqn.x" }), &d);
    try testing.expectEqual(@as(usize, 1), fe.config.set_fabric_endpoints.specs.n);
    const r = try refusal(av(&.{ "config", "set-fabric-endpoints", "sqmeta:///dev/m", "vol-0123456789abcdef=tcp:10.0.0.1:4420:nqn.x,tcp:10.0.1.1:4420:nqn.y" }));
    try testing.expect(r == .bad_endpoint_spec and r.bad_endpoint_spec.why == .subnqn_differs);
    var buf: [512]u8 = undefined;
    try testing.expect(std.mem.indexOf(u8, text(&buf, r), "'nqn.x' and 'nqn.y'") != null);
}

test "usage names every verb" {
    try testing.expect(std.mem.indexOf(u8, usage(null), "squeezefs format") != null);
    inline for (@typeInfo(std.meta.Tag(Verb)).@"enum".field_names) |name| {
        if (!std.mem.eql(u8, name, "version") and !std.mem.eql(u8, name, "help")) {
            const word = comptime if (std.mem.eql(u8, name, "claim_clear")) "squeezefs claim clear" else if (std.mem.eql(u8, name, "appender_clear")) "squeezefs appender clear" else if (std.mem.eql(u8, name, "fsck_merge")) "squeezefs fsck merge-reports" else "squeezefs " ++ name;
            try testing.expect(std.mem.indexOf(u8, usage(null), word) != null);
        }
    }
    try testing.expect(std.mem.indexOf(u8, usage(.mount), "--supervise") != null);
    try testing.expect(std.mem.indexOf(u8, usage(.umount), "<mountpoint>") != null);
}

fn wordParsersFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [256]u8 = undefined;
    const len = smith.slice(&buf);
    const word = buf[0..len];
    _ = parseUuid(word);
    _ = parseEndpointSpec(word);
    _ = parseShards(word);
    _ = parseJobIdWord(word);
    _ = parseTakeSlots(word);
    _ = parseSize(word);
}

test "fuzz: every argv word parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, wordParsersFuzz, .{ .corpus = &.{ fuzzkit.slice("c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1"), fuzzkit.slice("127.0.0.1:4420"), fuzzkit.slice("3/8"), fuzzkit.slice("0x7fffffffffffffff"), fuzzkit.slice("1-"), fuzzkit.slice("16G"), fuzzkit.slice("99999999999999999999T") } });
}
