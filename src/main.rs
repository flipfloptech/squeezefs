#![allow(clippy::style, clippy::complexity, clippy::pedantic)]

use clap::{Parser, Subcommand};
use colored::Colorize;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{start_mount, SqueezefsFilesystem};

use squeezefs::routing::DataRouter;
use squeezefs::FormatConfig;
use std::path::{Path, PathBuf};
use tokio::fs;
#[derive(Parser)]
#[command(name = "squeezefs")]
// Release-train + git-commit versioning (docs/operations.md §Versioning &
// releases): the line leads with the release-train version (CARGO_PKG_VERSION,
// bumped as a release act) AND carries the build-commit identity — both, not
// either. `-V`/`--version` print `squeezefs <train> (<commit identity>) ...`.
#[command(version = squeezefs::version::version_line())]
#[command(about = "Squeezefs: slimmed down high-performance distributed filesystem", long_about = None)]
struct Cli {
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Format metadata and data volumes to initialize squeezefs
    Format {
        /// Metadata and Data URIs (sqmeta://... and sqdata://...)
        #[arg(required = true)]
        uris: Vec<String>,
        /// Block size (e.g. "4M", "1M", default: 4MB)
        #[arg(long, default_value = "4M")]
        block_size: String,
        /// Formatted capacity (e.g. "100G"; default: summed physical size)
        ///
        /// The value may be lower than the physical size of the data
        /// volumes. Values above physical are rejected: oversubscription
        /// is not supported at the filesystem level. Thin-provision
        /// underneath, via LVM or the fabric, instead.
        #[arg(long)]
        capacity: Option<String>,
        /// Hard quota limiting the number of inodes (default: 1000000)
        #[arg(long, default_value = "1000000")]
        inodes: u64,
        /// Memory cache limit (default: "1GB")
        #[arg(long)]
        mem_cache_size: Option<String>,
        /// Disk cache limit (default: "10GB")
        #[arg(long, alias = "cache-size")]
        disk_cache_size: Option<String>,
        /// Disk read cache size limit (default: 50% of disk_cache_size)
        #[arg(long)]
        read_cache_size: Option<String>,
        /// Disk write staging size limit (default: 50% of disk_cache_size)
        #[arg(long)]
        write_cache_size: Option<String>,
        /// Memory read cache size limit (default: 50% of mem_cache_size)
        #[arg(long)]
        read_mem_cache_size: Option<String>,
        /// Memory write cache size limit (default: 50% of mem_cache_size)
        #[arg(long)]
        write_mem_cache_size: Option<String>,
        /// Comma-separated paths to local staging/cache directories
        #[arg(long, value_delimiter = ',', alias = "cache-dir")]
        disk_cache_paths: Option<Vec<PathBuf>>,
        /// SqueezeFS LVM/Physical Data Volume paths
        #[arg(
            long,
            value_delimiter = ',',
            alias = "data-lv",
            alias = "volume",
            alias = "backing-dev",
            alias = "nvme-target"
        )]
        data_lv: Option<Vec<String>>,

        /// Custom Shared-Block Metadata Backend ("MetaLV") paths
        #[arg(long, value_delimiter = ',')]
        meta_lv: Option<Vec<String>>,
        /// IP address of NVMe-oF target
        #[arg(long)]
        ip: Option<String>,
        /// Port of NVMe-oF target
        #[arg(long)]
        port: Option<u16>,
        /// Subsystem NQN of NVMe-oF target
        #[arg(long)]
        subnqn: Option<String>,
        /// Force formatting even if a squeezefs volume is already detected
        #[arg(long, short = 'f')]
        force: bool,
        /// Perform full zero-wiping of the entire backing device capacity (slow)
        #[arg(long)]
        full: bool,
        /// Compression algorithm (lz4, zstd, none, default: none)
        #[arg(long, default_value = "none")]
        compression: String,
        /// Encryption algorithm (aes256gcm, chacha20, none, default: none)
        #[arg(long, default_value = "none")]
        encrypt_algo: String,
        /// Path to the encryption key file (or "-" to read it from stdin)
        ///
        /// The file must hold at least 32 bytes of key material (not a
        /// passphrase) and be mode 0600, owned by you:
        /// `head -c 32 /dev/urandom | base64 > key && chmod 600 key`.
        /// The key never lands on the volume — only a derivation salt and
        /// a key id are persisted — and it is never passed on argv. Keep
        /// the file: mount needs it (`--encrypt-key`,
        /// SQUEEZEFS_ENCRYPT_KEY_FILE, or /etc/squeezefs/keys/<id>.key).
        #[arg(long, value_name = "PATH")]
        encrypt_key: Option<String>,
        /// Seconds to wait for staged writes to drain on dismount
        ///
        /// Staged writes drain to the block backend before the
        /// filesystem detaches. Default: 10.
        #[arg(long)]
        dismount_wait: Option<String>,
        /// Interval for background staging write uploads
        ///
        /// Accepts durations such as "500ms" or "5s".
        #[arg(long, default_value = "500ms")]
        upload_delay: String,
        /// Shared default io_uring SQPOLL idle timeout in milliseconds
        ///
        /// Applies to the /dev/fuse rings of every mount of this
        /// volume. Use 0 to disable the shared default.
        #[arg(long, env = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS")]
        fuse_io_uring_sqpoll_idle_ms: Option<u32>,
        /// Metadata btree node size in KiB: 64|128|256|512|1024
        ///
        /// Values below 256 print a warning: the per-volume record-value
        /// cap becomes node_size/4, trading xattr and layout headroom for
        /// cold-read latency.
        #[arg(long, default_value = "256")]
        meta_node_kib: u32,
        /// Metadata (v3) journal ring size in MiB, overriding the default
        /// clamp(volume/64, 8 MiB, 32 MiB).
        #[arg(long)]
        meta_journal_mb: Option<u64>,
        // RETIRED (design-dynamic-meta-routing §5.1, forward-only):
        // routing widths are DERIVED now, never chosen. The arg stays
        // declared so the refusal is ours (loud, naming the successor)
        // instead of clap's generic unknown-flag error.
        /// RETIRED: metadata routing width is derived, never chosen
        ///
        /// Every format freezes the derived virtual width (the full
        /// 65536-slot id namespace) and spreads minting so any volume's
        /// load is divisible from birth. Growth needs no planning: use
        /// `squeezefs volume add-meta --take-slots ...` (offline) or
        /// `squeezefs volume migrate-meta-slot` (online). Passing this
        /// flag is a hard error.
        #[arg(long, hide = true)]
        meta_slots: Option<u32>,
    },
    /// Show filesystem status
    Status {
        /// Optional Metadata URI (sqmeta://...) or mount point path
        meta_uri: Option<String>,
    },
    /// List client mount registrations on a metadata volume set
    ///
    /// Classifies each registration as live, stale, or dead from the
    /// on-volume heartbeat records. Read-only probe, safe to run beside
    /// a live mount.
    Clients {
        /// Metadata URI (sqmeta://...) or a metadata volume path
        meta_uri: String,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    // Anchors: design-volume-lifecycle §5.3–§5.5/§6 (PR VL3–VL5b).
    /// Manage the volume set: add, list, drain, remove, repair
    ///
    /// Data volumes: add, list, drain and remove, cancel a drain.
    /// Metadata volumes: add, remove, migrate routing slots, and repair
    /// the set-membership stamps.
    Volume {
        #[command(subcommand)]
        action: VolumeActions,
    },
    // Anchor: design-volume-lifecycle §6 (the maintenance-job fabric).
    /// Control maintenance jobs
    ///
    /// On a live mountpoint, commands act through the mount's admin
    /// lane. On a sqmeta:// URI, list and status are read-only probes
    /// of the durable job records.
    Job {
        #[command(subcommand)]
        action: JobActions,
    },
    // Anchors: design-volume-lifecycle §5.6 (PR VL6a) detection under
    // the suspects machinery, §5.6a (PR VL6b) per-class repair; offline
    // `--repair` takes the guarded D0 open.
    /// Check the filesystem, with optional per-class repair
    ///
    /// Runs seven check classes (C1-C7, as labeled in the report).
    /// Every finding is verified before it is reported, and detection
    /// never mutates the filesystem.
    ///
    /// TARGET is a live mountpoint or a sqmeta:// URI. On a mountpoint
    /// the scan reads the running daemon's live state. On a URI it uses
    /// offline read-only probes and fails while another writer holds
    /// the volumes; with --repair it takes the exclusive writer guard
    /// instead. TARGET may also be the literal `merge-reports`,
    /// followed by shard report files to combine.
    ///
    /// --repair plans per-class repairs of the verified findings and is
    /// a dry run unless --apply is also given. With --apply it executes
    /// quarantine-first actions on verified findings only. Exit status
    /// is nonzero when findings exist.
    Fsck {
        /// Live mountpoint, sqmeta:// URI, or `merge-reports`
        target: String,
        /// Shard report files (with `merge-reports`)
        #[arg(trailing_var_arg = true)]
        reports: Vec<String>,
        /// Force online (default when TARGET is a mountpoint)
        #[arg(long)]
        online: bool,
        /// Force offline (default when TARGET is a sqmeta:// URI)
        #[arg(long)]
        offline: bool,
        // Anchor: KD-3 (the duty-cycle throttle law).
        /// Duty-cycle throttle percentage (1–100; 100 = unthrottled)
        #[arg(long, default_value_t = 100)]
        throttle: u32,
        /// Emit the structured report as JSON
        #[arg(long)]
        json: bool,
        /// Offline sharding: scan the k-th of N shards ("k/N", 0-based)
        ///
        /// Zero-coordination ino-residue sharding. Union the shard
        /// outputs with `fsck merge-reports`. Incompatible with
        /// --repair.
        #[arg(long)]
        shards: Option<String>,
        // Anchor: KD-17 (scrub verification depth per data class).
        /// Add the data scrub (check class C7)
        ///
        /// AEAD verification on encrypted data, frame decode on
        /// compressed data, readability checks on plain data.
        #[arg(long)]
        scrub: bool,
        // Anchor: the §5.6a per-class repair table.
        /// Plan per-class repairs (a dry run unless --apply is given)
        ///
        /// Repair is a writer: offline it requires the exclusive
        /// guarded open.
        #[arg(long)]
        repair: bool,
        /// Execute the repair plan (with --repair)
        ///
        /// Actions are quarantine-first, verify-before-repair, and
        /// idempotent per class.
        #[arg(long)]
        apply: bool,
        /// Quarantine directory (default: <first staging dir>/quarantine/)
        ///
        /// Cache-less filesystems have no staging directory, so this
        /// option is required there.
        #[arg(long)]
        quarantine_dir: Option<String>,
    },
    // Anchors: design-volume-lifecycle §5.6 / KD-17.
    /// Run the data scrub (check class C7)
    ///
    /// The standalone spelling of `fsck --scrub`, using the same
    /// engine: AEAD verification on encrypted data, frame decode on
    /// compressed data, readability checks on plain data.
    Scrub {
        /// Live mountpoint or sqmeta:// URI
        target: String,
        // Anchor: KD-3 (the duty-cycle throttle law).
        /// Duty-cycle throttle percentage (1–100; 100 = unthrottled)
        #[arg(long, default_value_t = 100)]
        throttle: u32,
        /// Emit the structured report as JSON
        #[arg(long)]
        json: bool,
    },
    // Anchors: design-volume-lifecycle §5.7 (KD-11 four-axis model,
    // KD-12 rebalance surface), §5.8 offline D0-guarded coordinator;
    // the movers reuse the VL4 move engine (contiguity-aware pick), the
    // W2 fold, and the KV SMO compactor.
    /// Defragment the filesystem along four measured axes
    ///
    /// Fragmentation is measured on four axes: D1 free-space
    /// contiguity, D2 file locality, D3 staged-extent pressure, and D4
    /// metadata node occupancy. Each axis has a gauge (the stats
    /// inode's frag_d1..frag_d4 fields) and an independently invocable
    /// mover.
    ///
    /// TARGET is a live mountpoint or a sqmeta:// URI. On a mountpoint
    /// the movers run as jobs on the mounted daemon through the admin
    /// lane. On a URI, a short-lived offline coordinator takes the
    /// exclusive writer guard and runs the job in-process. --fold works
    /// on live mounts only: extent-fold custody belongs to the mount.
    Defrag {
        /// Live mountpoint or sqmeta:// URI
        target: String,
        /// D1/D2: compact free space + rewrite poor-locality files
        #[arg(long)]
        data: bool,
        /// Restrict --data to one volume id (see `squeezefs volume list`)
        #[arg(long)]
        volume: Option<String>,
        // Anchor: the KV SMO compactor (design-cow-kv-metadata).
        /// D4: compact metadata btree nodes that are heavy with dead
        /// entries
        #[arg(long)]
        meta: bool,
        /// D3: kick parked/spilled extents through the fold machinery
        #[arg(long)]
        fold: bool,
        // Anchor: KD-12 — the pass is the VL4 mover's rebalance mode
        // (the same engine `volume add-data` schedules by default).
        /// Rebalance block placement across the data volumes (the same
        /// pass `volume add-data` schedules by default)
        #[arg(long)]
        rebalance: bool,
        /// Measure the four axes and print the per-volume/per-axis
        /// report; moves nothing
        #[arg(long)]
        report_only: bool,
        // Anchor: KD-3 (the duty-cycle throttle law).
        /// Duty-cycle throttle percentage for the mover jobs (1–100;
        /// 100 = unthrottled)
        #[arg(long, default_value_t = 100)]
        throttle: u32,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    // Anchor: docs/design-metadata-throughput.md §5.0 (the D0
    // single-writer mount guard).
    /// Single-writer mount-guard claim administration
    Claim {
        #[command(subcommand)]
        action: ClaimActions,
    },
    /// Mount squeezefs at a target path
    Mount {
        /// Metadata URIs (sqmeta://...) and Mountpoint path (last argument)
        #[arg(required = true)]
        args: Vec<String>,

        /// Memory cache limit (e.g., "128GB" or "50%")
        #[arg(long)]
        mem_cache_size: Option<String>,

        /// Disk cache limit (e.g., "200GB" or "80%")
        #[arg(long, alias = "cache-size")]
        disk_cache_size: Option<String>,

        /// Disk read cache size limit
        #[arg(long)]
        read_cache_size: Option<String>,

        /// Disk write staging size limit
        #[arg(long)]
        write_cache_size: Option<String>,

        /// Memory read cache size limit
        #[arg(long)]
        read_mem_cache_size: Option<String>,

        /// Memory write cache size limit
        #[arg(long)]
        write_mem_cache_size: Option<String>,
        // Anchor: docs/design-read-path.md §5.7 (the R5 joint memory
        // budget authority).
        /// Joint memory budget for the daemon's caches and buffers
        ///
        /// Accepts sizes such as "6G". Overrides SQUEEZEFS_MEM_BUDGET_MB
        /// and the cgroup-derived default.
        #[arg(long)]
        mem_budget: Option<String>,

        /// Custom Shared-Block Metadata Backend ("MetaLV") paths
        #[arg(long, value_delimiter = ',')]
        meta_lv: Option<Vec<String>>,

        /// Rejected at mount time: cache paths are fixed at format
        ///
        /// Cache paths are recorded in the format config and cannot be
        /// overridden at mount. Passing this flag is an error, never a
        /// silent ignore. Use `squeezefs config set-cache-paths` to
        /// change them.
        #[arg(long, value_delimiter = ',', alias = "cache-dir")]
        disk_cache_paths: Option<Vec<PathBuf>>,

        /// SqueezeFS LVM/Physical Data Volume paths
        #[arg(
            long,
            value_delimiter = ',',
            alias = "data-lv",
            alias = "volume",
            alias = "backing-dev",
            alias = "nvme-target"
        )]
        data_lv: Option<Vec<String>>,
        /// IP address of NVMe-oF target
        #[arg(long)]
        ip: Option<String>,
        /// Port of NVMe-oF target
        #[arg(long)]
        port: Option<u16>,
        /// Subsystem NQN of NVMe-oF target
        #[arg(long)]
        subnqn: Option<String>,

        /// Run FUSE daemon in the background (detach from terminal)
        #[arg(long)]
        daemon: bool,

        /// Keep the parent alive as a mount watchdog (requires --daemon)
        ///
        /// The parent probes <mountpoint>/.stats every 5s
        /// (SQUEEZEFS_SUPERVISE_INTERVAL_SECS). After 30s of sustained
        /// unresponsiveness (SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS) it
        /// logs an error, dumps daemon state, and, when run as root,
        /// writes /sys/fs/fuse/connections/<id>/abort to release
        /// blocked callers. This complements the in-daemon watchdog,
        /// which can log a wedge but not clear one. Kill and restart
        /// stay manual; the escalation prints the daemon PID and the
        /// exact commands.
        #[arg(long, requires = "daemon")]
        supervise: bool,

        /// UID presented as the owner of files in the mount
        ///
        /// Default: the current user, or SUDO_UID under sudo.
        /// Presentation-only: staging and cache I/O still run as the
        /// user executing `squeezefs mount`. The staging preflight
        /// checks that identity can write the roots; format and
        /// set-cache-paths stamp their ownership.
        #[arg(long)]
        uid: Option<u32>,

        /// GID presented as the group of files in the mount
        ///
        /// Default: the current group, or SUDO_GID under sudo.
        /// Presentation-only, like --uid.
        #[arg(long)]
        gid: Option<u32>,

        /// Disable FUSE writeback cache (enabled by default)
        #[arg(long)]
        no_writeback: bool,

        // Anchors: docs/design-preload-interception.md (the L4 program)
        // — KD-11 forced write-through (§5.6.2), the §5.2 session-host
        // control plane.
        /// Enable the LD_PRELOAD interception session host for this mount
        ///
        /// Equivalent to `-o interception` / SQUEEZEFS_IPC=1. Clients
        /// launched with LD_PRELOAD=libsqueezefs_il.so exchange data
        /// with the daemon directly, bypassing the kernel. Forces
        /// kernel write-through on the mount; combining it with an
        /// explicit writeback request is an error.
        #[arg(long)]
        interception: bool,

        /// Allow other users to access the mount
        #[arg(long, alias = "allow-others")]
        allow_other: bool,

        /// Validate backend storage connectivity on startup
        #[arg(long)]
        check_storage: bool,

        /// Seconds to wait for staged writes to drain on dismount
        ///
        /// Staged writes drain to the block backend before the
        /// filesystem detaches. Default: 10.
        #[arg(long)]
        dismount_wait: Option<String>,
        /// Interval for background staging write uploads
        ///
        /// Accepts durations such as "500ms" or "5s".
        #[arg(long)]
        upload_delay: Option<String>,

        /// io_uring SQPOLL idle timeout in milliseconds for this mount
        ///
        /// Overrides the volume's shared default for the /dev/fuse
        /// rings. Use 0 to disable even when the volume has a shared
        /// default.
        #[arg(long, env = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS")]
        fuse_io_uring_sqpoll_idle_ms: Option<u32>,

        /// Pin the io_uring SQPOLL kernel thread to a CPU (this mount)
        ///
        /// Applies to the /dev/fuse rings. Use 0 to disable CPU
        /// pinning.
        #[arg(long, env = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU")]
        fuse_io_uring_sqpoll_cpu: Option<u32>,

        // Anchor: docs/design-key-handling.md §4 (VAL-3 — the key
        // resolves at mount; the volume stores only a salt and an id).
        /// Path to the encryption key file for an encrypted volume
        ///
        /// Required to mount a volume formatted with --encrypt-algo,
        /// unless the key is at SQUEEZEFS_ENCRYPT_KEY_FILE or
        /// /etc/squeezefs/keys/<key_id>.key. "-" reads it from stdin
        /// (read before --daemon forks). Mode 0600, owned by you.
        #[arg(long, value_name = "PATH")]
        encrypt_key: Option<String>,

        /// Custom FUSE options (comma-separated list, e.g. "ro,nonempty")
        #[arg(short = 'o', long)]
        options: Option<String>,

        /// Background job worker CPU utilization limit, in percent
        ///
        /// Accepts 1 to 100. Default: 50.
        #[arg(long, default_value_t = 50)]
        job_cpu_limit: u32,

        /// Enable read-after-write checksum verification on writes to cache and disk
        #[arg(long)]
        write_verification: bool,

        // Anchor: P2-9 (sampled read-after-write verification).
        /// With --write-verification, verify every N-th write
        ///
        /// The default of 1 verifies every write. Larger values reduce
        /// the read-after-write cost under load.
        #[arg(long, default_value_t = 1)]
        write_verification_sample: u64,
    },
    /// Unmount a squeezefs mountpoint
    ///
    /// Unmounts via fusermount or umount. With -f it kills holders and
    /// any leftover daemon, falling back to a lazy umount as a last
    /// resort.
    Umount {
        /// Optional Metadata URI (sqmeta://...)
        #[arg(
            long,
            short = 'g',
            env = "SQUEEZEFS_META_URI",
            alias = "meta-uri",
            alias = "meta_uri"
        )]
        meta_uri: Option<String>,
        /// Path to the mountpoint
        mountpoint: PathBuf,
        /// Skip drain prompts; kill holders / leftover daemon; last resort uses umount -l
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Benchmark a mounted filesystem
    ///
    /// Runs over a persistent, reusable dataset at
    /// <MOUNTPOINT>/squeezefs-bench (simplified-elbencho model).
    ///
    /// A bare invocation (no phase flags) runs the full saturation
    /// suite over one auto-sized dataset: write seq 1m --direct, read
    /// seq 1m --direct, read rand 4k --direct (30s box), write rand 4k
    /// --direct (30s box), stat, del. The del pass leaves the mount
    /// clean.
    ///
    /// Explicit phase flags run exactly those phases in the fixed
    /// order write, read, stat, del, with the same auto defaults for
    /// -t/-n/-s, so single-phase numbers stay comparable to the suite
    /// passes.
    Bench {
        /// Path to the mounted filesystem directory
        mountpoint: PathBuf,
        /// Write phase: create or overwrite the dataset, timed
        ///
        /// Timing includes file create/open, and every file is fsync'd
        /// before the clock stops, so the rows report durable writes.
        #[arg(short = 'w', long)]
        write: bool,
        /// Read phase: read the dataset back, timed
        ///
        /// Reuses the dataset from an earlier -w; fails if its shape
        /// does not match.
        #[arg(short = 'r', long)]
        read: bool,
        /// Stat phase: stat every dataset file, timed
        #[arg(long)]
        stat: bool,
        /// Delete phase: delete the dataset, timed (doubles as cleanup)
        #[arg(long)]
        del: bool,
        /// Number of worker threads [default: auto = min(CPUs, 16)]
        ///
        /// Worker t owns squeezefs-bench/t{t}/.
        #[arg(short = 't', long)]
        threads: Option<usize>,
        /// Files per thread [default: auto = 1]
        #[arg(short = 'n', long)]
        files: Option<usize>,
        /// File size: human units 4k / 128k / 4m / 10g, or plain bytes
        ///
        /// Default: auto-sized. The total is max(16g, 2g x threads),
        /// capped at 25% of the mountpoint's free space and rounded
        /// down to 1 MiB.
        #[arg(short = 's', long, value_parser = squeezefs::bench::parse_size)]
        size: Option<u64>,
        /// I/O block size per operation, same units [default: 1m]
        ///
        /// Explicit phase runs only; the suite fixes 1m sequential and
        /// 4k random.
        #[arg(short = 'b', long, value_parser = squeezefs::bench::parse_size)]
        block: Option<u64>,
        /// Random offsets (explicit phase runs only)
        ///
        /// Uses a shuffled full-coverage block list: every block
        /// exactly once.
        #[arg(long)]
        rand: bool,
        /// O_DIRECT I/O
        ///
        /// Requires -b to be a multiple of 4096 and -s to be a multiple
        /// of -b; anything else is an error. The suite's I/O passes are
        /// always O_DIRECT.
        ///
        /// Hybrid I/O note: on a default mount, O_DIRECT reads serve
        /// from the SqueezeFS read tiers once warm. For device-path or
        /// amplification measurement, mount with `-o direct_device_true`.
        /// The bench header prints which posture the rows carry.
        #[arg(long)]
        direct: bool,
        /// Wall-clock time box in seconds for rand read/write passes
        ///
        /// Default: 30 for --rand, unlimited (full coverage) for
        /// sequential; 0 forces full coverage. Partial coverage is
        /// stated in the results row.
        #[arg(long)]
        time: Option<u64>,
        /// Repeat the selected pass set N times
        ///
        /// Each iteration is timed fresh; write iterations overwrite
        /// the dataset in place.
        #[arg(short = 'i', long, default_value_t = 1)]
        iterations: usize,
    },
    /// Clone a file metadata-only (instant Copy-on-Write cloning)
    ///
    /// Runs offline under the exclusive writer guard (refused while the
    /// set is mounted): the clone's map names the source's blocks and
    /// their reference counts rise — no data is copied. A file whose
    /// payload is still in a mount's local staging cannot be cloned
    /// offline; clone it on the live mount (`cp --reflink=always`).
    Clone {
        /// Metadata URI (sqmeta://...) — required
        ///
        /// Both paths resolve through this metadata set, and the
        /// writer guard is taken over it for the clone's duration.
        #[arg(
            long,
            short = 'g',
            env = "SQUEEZEFS_META_URI",
            alias = "meta-uri",
            alias = "meta_uri"
        )]
        meta_uri: Option<String>,
        /// Source file path
        src: String,
        /// Destination file path
        dest: String,
    },
    /// Automatically tune client node configuration
    ///
    /// Applying changes requires root (run under sudo).
    Tune,
    /// Configuration management utility
    Config {
        /// Optional Metadata URI (sqmeta://...)
        #[arg(
            long,
            short = 'g',
            env = "SQUEEZEFS_META_URI",
            alias = "meta-uri",
            alias = "meta_uri"
        )]
        meta_uri: Option<String>,
        #[command(subcommand)]
        action: ConfigActions,
    },
    /// Show filesystem space and inode usage for a volume set
    ///
    /// An offline query over the durable state, addressed by URI. A
    /// mounted filesystem also answers plain `df -h <mountpoint>` via
    /// statfs.
    Df {
        /// Metadata URI (sqmeta://...) or a metadata volume path
        #[arg(
            long,
            short = 'g',
            env = "SQUEEZEFS_META_URI",
            alias = "meta-uri",
            alias = "meta_uri"
        )]
        meta_uri: Option<String>,
        /// Metadata URI (sqmeta://...) or metadata volume path
        /// (positional alternative to --meta-uri)
        path: Option<String>,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage underlying LVM storage pools and volumes
    Storage {
        #[command(subcommand)]
        action: StorageActions,
    },
    /// Manage NVMe over Fabrics target shares and client connections
    ///
    /// Dual-stack: SPDK is the default target stack; select kernel
    /// nvmet with --target-stack nvmet.
    Nvmeof {
        #[command(subcommand)]
        action: NvmeofActions,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ClaimActions {
    /// Remove a stale writer claim (operator-attested)
    ///
    /// The recovery step for a crashed cross-host writer on a volume
    /// without NVMe Persistent Reservations. Fresh claims and
    /// live-mounted volumes are refused, and staleness is re-verified
    /// under the command's own probe.
    ///
    /// The automation ladder ahead of this verb: same-host dead-pid
    /// auto-reclaim, then Persistent-Reservation preempt, then claim
    /// TTL expiry. No mount flag bypasses the guard.
    Clear {
        /// Metadata URI (sqmeta://...)
        ///
        /// Every volume in the set is cleared in order.
        meta_uri: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum StorageActions {
    /// Manage storage pools (LVM Volume Groups)
    #[command(subcommand)]
    Pool(StoragePoolActions),

    /// Manage storage volumes (LVM Logical Volumes)
    #[command(subcommand)]
    Volume(StorageVolumeActions),
}

#[derive(Subcommand, Debug, Clone)]
enum StoragePoolActions {
    /// Create a new storage pool from physical disks
    Create {
        /// Pool name
        pool_name: String,
        /// Physical disk paths (e.g. /dev/nvme0n1 /dev/nvme1n1)
        disks: Vec<String>,
    },
    /// Add physical disks to an existing storage pool
    Add {
        /// Pool name
        pool_name: String,
        /// Physical disk paths
        disks: Vec<String>,
    },
    /// Remove physical disks from a storage pool
    Remove {
        /// Pool name
        pool_name: String,
        /// Physical disk paths to remove
        disks: Vec<String>,
        /// Automatically bypass interactive confirmations
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Delete an entire storage pool
    Delete {
        /// Pool name
        pool_name: String,
        /// Automatically bypass interactive confirmations
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List all storage pools (Volume Groups)
    List,
}

#[derive(Subcommand, Debug, Clone)]
enum StorageVolumeActions {
    /// Create a storage volume in a pool
    Create {
        /// Pool name
        pool_name: String,
        /// Volume name
        vol_name: String,
        /// Volume size (e.g. 1P, 100G)
        #[arg(long)]
        size: String,
        /// Number of disks/stripes to stripe across
        ///
        /// Default: auto-detect all disks in the pool.
        #[arg(long)]
        stripes: Option<usize>,
        /// Stripe size (e.g. 64K, 256K, 512K, default: 512K)
        #[arg(long)]
        stripe_size: Option<String>,
    },
    /// Extend a storage volume
    Extend {
        /// Pool name
        pool_name: String,
        /// Volume name
        vol_name: String,
        /// Size to add (e.g. 500G, 10T)
        #[arg(long)]
        add_size: String,
    },
    /// Delete a storage volume
    Delete {
        /// Pool name
        pool_name: String,
        /// Volume name
        vol_name: String,
        /// Automatically bypass interactive confirmations
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List all storage volumes (Logical Volumes)
    List,
}

/// `--target-stack` argument (§6.2 of the NVMe-oF target-management
/// design): explicit selection, default `spdk`, loud failure — never a
/// silent cross-stack fallback.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum TargetStackArg {
    Spdk,
    Nvmet,
}

impl From<TargetStackArg> for squeezefs::nvmeof::StackKind {
    fn from(v: TargetStackArg) -> Self {
        match v {
            TargetStackArg::Spdk => squeezefs::nvmeof::StackKind::Spdk,
            TargetStackArg::Nvmet => squeezefs::nvmeof::StackKind::Nvmet,
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
enum NvmeofActions {
    /// Share a local block device or file as an NVMe-oF subsystem
    ///
    /// The default target stack is spdk; select kernel nvmet with
    /// --target-stack nvmet.
    Share {
        /// Local backing path (e.g. /dev/nvme1n1 or /srv/backing.img)
        backing_path: String,
        /// Optional custom Subsystem NQN (default:
        /// nqn.2026-07.io.squeezefs:share-<uuid>)
        #[arg(long)]
        subnqn: Option<String>,
        /// Port to bind target listeners to (default: 4420)
        #[arg(long, default_value_t = 4420)]
        port: u16,
        /// IP address(es) to bind listeners to (comma-separated or repeated)
        #[arg(long, required = true, value_delimiter = ',')]
        ip: Vec<String>,
        /// Target stack (flag > SQUEEZEFS_NVMEOF_TARGET_STACK env > default spdk)
        #[arg(long, value_enum)]
        target_stack: Option<TargetStackArg>,
        /// Namespace id (SPDK only)
        ///
        /// The kernel-nvmet namespace index is structurally fixed at 1;
        /// other values with nvmet are an error.
        #[arg(long)]
        nsid: Option<u32>,
        /// Namespace identity UUID
        ///
        /// Recorded and re-presented by restore on both stacks.
        /// Generated once at share time when absent.
        #[arg(long)]
        ns_uuid: Option<String>,
        /// Create a missing file backing as a sparse file of this size
        ///
        /// Accepts sizes such as "10G". Without this flag a missing
        /// backing is an error.
        #[arg(long)]
        create_size: Option<String>,
        /// Allow only these host NQNs to connect (repeatable)
        ///
        /// Default: allow any host, the trusted-fabric posture.
        #[arg(long)]
        allow_host: Vec<String>,
        /// Proceed despite SPDK target version drift from the pin
        ///
        /// SPDK only; an error with --target-stack nvmet.
        #[arg(long)]
        accept_version_drift: bool,
    },
    /// Stop sharing a target subsystem
    ///
    /// The stack is resolved from the share ledger, never guessed.
    /// Unledgered NQNs are an error.
    Unshare {
        /// Subsystem NQN to unshare
        subnqn: String,
        /// Tear down even with live initiator connections
        ///
        /// The SPDK stack detects live connections and refuses without
        /// this flag.
        #[arg(long)]
        force: bool,
        /// Proceed despite SPDK target version drift from the pin
        ///
        /// SPDK only; an error on nvmet-recorded NQNs.
        #[arg(long)]
        accept_version_drift: bool,
    },
    /// List ledgered shares and connected remote fabric disks
    ///
    /// Shares are reconciled against live target state and reported as
    /// managed, down, pending, removing, or foreign.
    List {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Re-establish ledgered shares on their recorded stacks
    ///
    /// Idempotent. Reconciles interrupted share/unshare intents and
    /// prints a per-share report.
    Restore {
        /// Replay only records recorded for this stack
        ///
        /// A filter, never a retarget.
        #[arg(long, value_enum)]
        target_stack: Option<TargetStackArg>,
        /// Proceed despite SPDK target version drift from the pin
        ///
        /// SPDK only; an error with --target-stack nvmet.
        #[arg(long)]
        accept_version_drift: bool,
    },
    /// Absorb a live unledgered share into management
    ///
    /// An explicit operator action: writes only the share ledger and
    /// never touches the live target object. The stack is auto-detected
    /// from where the subsystem lives.
    Adopt {
        /// Subsystem NQN to adopt (must be live on exactly one stack)
        subnqn: String,
        /// Disambiguate an NQN that is live on both stacks
        ///
        /// Without this flag such an adopt fails closed, naming both
        /// holders. Never a retarget.
        #[arg(long, value_enum)]
        target_stack: Option<TargetStackArg>,
    },
    /// Connect local client to a remote NVMe-oF target
    Connect {
        /// Remote target IP address
        #[arg(long)]
        ip: String,
        /// Remote target port (default: 4420)
        #[arg(long, default_value_t = 4420)]
        port: u16,
        /// Remote target Subsystem NQN
        #[arg(long)]
        subnqn: String,
        /// Source address for the fabric connection
        ///
        /// Binds the connection to the given local address. Required
        /// when several local interfaces share a subnet and routing
        /// alone cannot select the outgoing path; on distinct subnets
        /// routing usually suffices.
        #[arg(long)]
        host_traddr: Option<String>,
        /// Source network interface for the fabric connection
        ///
        /// Binds the connection to the named local interface. Combine
        /// with --host-traddr to pin both the address and the
        /// interface.
        #[arg(long)]
        host_iface: Option<String>,
        /// Upper bound on the number of I/O queues to request
        ///
        /// A target may offer fewer I/O queues than the initiator
        /// requests by default, which fails the connection; this flag
        /// caps the request at the target's limit. Minimum 1.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        nr_io_queues: Option<u32>,
    },
    /// Disconnect local client from a remote NVMe-oF target
    Disconnect {
        /// Subsystem NQN to disconnect
        subnqn: String,
    },
    /// Manage the NVMe-oF target runtime
    ///
    /// SPDK lifecycle: pinned install, hugepage setup,
    /// start/stop/status, systemd-unit emission. The nvmet stack
    /// implements the verbs where they are meaningful.
    Target {
        #[command(subcommand)]
        action: TargetActions,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum TargetActions {
    /// Build the pinned SPDK release from source
    ///
    /// Verifies the tag and commit sha, then builds into
    /// /opt/squeezefs/spdk/<tag>/. SPDK only by definition.
    Install {
        /// Release to install; must equal the pinned tag
        ///
        /// Pin bumps are deliberate source changes, never a CLI flag.
        #[arg(long)]
        version: Option<String>,
        /// Consent to system package mutation
        ///
        /// Runs the pinned tree's scripts/pkgdep.sh. Default: probe the
        /// toolchain and fail, listing the missing packages.
        #[arg(long)]
        with_pkgdep: bool,
    },
    /// Prepare the host for the selected target stack
    ///
    /// SPDK: reserves 2 MiB hugepages and records the prior value;
    /// restore it with --restore-prior. nvmet: modprobe and configfs
    /// mount checks.
    Setup {
        /// Hugepage reservation in MiB (default 2048 = 1024 × 2 MiB pages)
        #[arg(long, conflicts_with = "restore_prior")]
        hugemem_mb: Option<u64>,
        /// Restore nr_hugepages to the recorded prior value and clear the
        /// record
        #[arg(long)]
        restore_prior: bool,
        /// Target stack (flag > SQUEEZEFS_NVMEOF_TARGET_STACK env > default spdk)
        #[arg(long, value_enum)]
        target_stack: Option<TargetStackArg>,
    },
    /// Start the target
    ///
    /// SPDK runs in pidfile mode: preflighted spawn, RPC-liveness
    /// wait, then load_config. nvmet: modprobe and ledger restore.
    Start {
        /// Target stack (flag > SQUEEZEFS_NVMEOF_TARGET_STACK env > default spdk)
        #[arg(long, value_enum)]
        target_stack: Option<TargetStackArg>,
        /// Explicit reactor core mask (hex, e.g. 0x80000000)
        ///
        /// Default: one reactor on the highest online CPU.
        #[arg(long, conflicts_with = "cores")]
        core_mask: Option<String>,
        /// Reactor core count, allocated from the highest online CPUs down
        #[arg(long)]
        cores: Option<u32>,
        /// DPDK hugepage memory for spdk_tgt -s, in MiB (default 1024)
        #[arg(long)]
        dpdk_mem_mb: Option<u64>,
        /// Proceed despite a target version that drifts from the pin
        ///
        /// A gate on mutating verbs only: status always reports, stop
        /// warns.
        #[arg(long)]
        accept_version_drift: bool,
    },
    /// Stop the target
    ///
    /// SPDK sequence: save_config, SIGTERM, a grace period, then
    /// SIGKILL. Fails while ledgered shares have live consumers unless
    /// --force is given.
    Stop {
        /// Stop even with live initiator connections on ledgered shares
        #[arg(long)]
        force: bool,
        /// Target stack (flag > SQUEEZEFS_NVMEOF_TARGET_STACK env > default spdk)
        #[arg(long, value_enum)]
        target_stack: Option<TargetStackArg>,
    },
    /// Report target health
    ///
    /// Covers RPC liveness, version and drift from the pin, reactor
    /// busy, hugepages, and ledger reconciliation. Never fails on
    /// version drift.
    Status {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Target stack (flag > SQUEEZEFS_NVMEOF_TARGET_STACK env > default spdk)
        #[arg(long, value_enum)]
        target_stack: Option<TargetStackArg>,
    },
    /// Emit a systemd unit to stdout
    ///
    /// Values are baked at emission time. The unit is never installed;
    /// the operator installs it.
    SystemdUnit {
        /// Target stack (flag > SQUEEZEFS_NVMEOF_TARGET_STACK env > default spdk)
        #[arg(long, value_enum)]
        target_stack: Option<TargetStackArg>,
        /// Explicit reactor core mask (hex) to bake into ExecStart
        #[arg(long, conflicts_with = "cores")]
        core_mask: Option<String>,
        /// Reactor core count from the highest online CPUs down
        #[arg(long)]
        cores: Option<u32>,
        /// DPDK hugepage memory (spdk_tgt -s) to bake, in MiB (default 1024)
        #[arg(long)]
        dpdk_mem_mb: Option<u64>,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum VolumeActions {
    // Anchors: design-volume-lifecycle §5.3 (PR VL3); the add stamps the
    // KV_VOLUME_LIFECYCLE incompat bit (pre-VL3 binaries refuse it loud).
    /// Add a data volume to the set
    ///
    /// The new member gets a durable, never-reused `vol-` id. The add
    /// is preflight-checked: the device must exist, be sized, not
    /// already be a member, and pass a write-and-readback probe.
    ///
    /// TARGET is a live mountpoint (online add through the admin lane)
    /// or a sqmeta:// URI (offline; guarded like `config
    /// set-cache-paths`). The add marks the volume set: older squeezefs
    /// releases refuse to open it afterwards.
    AddData {
        /// Live mountpoint or sqmeta:// URI
        target: String,
        /// Backing device to add (block device or file)
        device: String,
        // Anchor: the §5.3 step-6 default rebalance (the VL4 mover).
        /// Opt out of the automatic rebalance pass after the add
        #[arg(long)]
        no_rebalance: bool,
    },
    /// List the durable volume set
    ///
    /// Prints ids, backing devices, states, and the capacity figures
    /// available from the query path used: live mounts report used and
    /// free space from the allocators; offline probes report device
    /// capacity. `squeezefs df` prints the full census.
    List {
        /// Live mountpoint or sqmeta:// URI
        target: String,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    // Anchors: design-volume-lifecycle §5.4 (drain/remove), §5.2
    // capacity census, §5.8 offline D0-guarded coordinator.
    /// Drain and remove a data volume
    ///
    /// A capacity preflight runs first; when the surviving volumes
    /// cannot hold the data census, the command fails and prints the
    /// capacity figures. The volume then flips durably from active to
    /// draining: it is excluded from new placement but still serves
    /// reads. Copy-on-write evacuation moves the data (shared clone
    /// blocks move once), and the volume retires when its census
    /// reaches zero.
    ///
    /// TARGET is a live mountpoint (the drain runs on the mounted
    /// daemon) or a sqmeta:// URI (offline: a short-lived coordinator
    /// takes the exclusive writer guard and drains in-process to
    /// completion).
    RemoveData {
        /// Live mountpoint or sqmeta:// URI
        target: String,
        /// Durable volume id (see `squeezefs volume list`)
        volume_id: String,
        /// Duty-cycle throttle percentage for the evacuation job
        #[arg(long, default_value_t = 100)]
        throttle: u32,
    },
    // Anchor: KD-5 (volume ids are permanent, never reused).
    /// Cancel an in-flight drain
    ///
    /// The volume returns to `active` and to write placement, and the
    /// evacuation job is cancelled cleanly. Retired volumes never come
    /// back; their ids are permanent.
    Undrain {
        /// Live mountpoint or sqmeta:// URI
        target: String,
        /// Durable volume id
        volume_id: String,
    },
    // Anchors: design-volume-lifecycle §5.5.1a/§5.5.2b (PR VL5b);
    // offline it takes the D0 writer-guard claims.
    /// Inspect and reconcile the metadata set-membership stamps
    ///
    /// Prints the observed per-volume stamp state. A coherent observed
    /// state is re-stamped idempotently, including the single
    /// inferable missing member a crashed repair can leave. Slot-flip
    /// epoch spreads resolve by per-slot highest-epoch-wins. Any other
    /// state fails with an explanation and no changes.
    ///
    /// Offline verb: takes the exclusive writer-guard claims like
    /// format-grade verbs, and fails while the set is live-mounted.
    RepairSet {
        /// sqmeta:// URI of the metadata volume set
        target: String,
    },
    // Anchors: design-volume-lifecycle §5.5.2 (PR VL5b); the KD-8
    // staging-drain barrier; offline D0-guarded coordinator.
    /// Add a metadata volume and migrate routing slots onto it
    ///
    /// Formats the device as a new member and migrates the taken
    /// routing slots onto it; an added metadata volume is only useful
    /// together with slot migration. Staged writes are drained first,
    /// and the filesystem generation is restamped.
    ///
    /// Offline verb: unmount first and pass the sqmeta:// URI of the
    /// current set. The coordinator takes the exclusive writer guard.
    /// Idempotent: re-run an interrupted add with the same arguments.
    AddMeta {
        /// sqmeta:// URI of the current metadata set
        target: String,
        /// Blank backing device for the new member
        device: String,
        /// Slots to take: a count ("2" = the 2 most-loaded takeable
        /// slots) or an explicit list ("1,3,5")
        #[arg(long, default_value = "1")]
        take_slots: String,
    },
    // Anchors: design-volume-lifecycle §5.5.2 (PR VL5b) — bulk copy +
    // conveyor delta tee + the §5.5.2a cutover gate + the §5.5.2b flip.
    /// Migrate one routing slot to another metadata volume
    ///
    /// Runs on a live mount: bulk copy, then live-delta catch-up, then
    /// a brief cutover pause (p99 window under 250 ms target) and the
    /// durable flip. Runs as a background job; `squeezefs job list`
    /// shows progress. An idempotent re-run converges after any crash.
    MigrateMetaSlot {
        /// Live mountpoint
        target: String,
        /// The routing slot to move
        slot: u16,
        /// Target metadata volume index (canonical member order)
        target_volume: usize,
    },
    // Anchors: design-volume-lifecycle §5.5.2, §5.2 meta-side capacity
    // preflight, KD-8 generation barrier; offline D0-guarded coordinator.
    /// Remove a metadata volume from the set
    ///
    /// Migrates every routing slot the victim hosts to the surviving
    /// members. A capacity preflight runs first and fails, with the
    /// figures printed, when the survivors cannot fit the slots. The
    /// survivors are stamped first, the victim's retirement tombstone
    /// last, and the filesystem generation is restamped.
    ///
    /// Offline verb: unmount first. The coordinator takes the
    /// exclusive writer guard. An idempotent re-run converges.
    RemoveMeta {
        /// sqmeta:// URI of the current set (victim included)
        target: String,
        /// The victim member's device path (as listed in the URI)
        victim: String,
    },
}

#[derive(Subcommand)]
enum JobActions {
    /// List jobs
    ///
    /// TARGET is a live mountpoint, or a sqmeta:// URI for the offline
    /// read-only probe.
    List { target: String },
    /// One job's status
    Status { target: String, job_id: String },
    /// Pause a running job (live mount only)
    Pause { target: String, job_id: String },
    /// Resume a paused job (live mount only)
    Resume { target: String, job_id: String },
    /// Cancel a job (terminal; live mount only)
    Cancel { target: String, job_id: String },
    /// Retune a job's duty-cycle percentage live (live mount only)
    Throttle {
        target: String,
        job_id: String,
        pct: u32,
    },
    // Anchor: design-volume-lifecycle §5.1.6 (the remote-worker wire).
    /// Enroll this client as a remote data-plane job worker
    ///
    /// Probes the mount registrations for the live coordinator's job
    /// endpoint, proves storage membership via the enrollment secret,
    /// and serves job shards until the coordinator goes away.
    Worker {
        /// Metadata URI (sqmeta://...) of the volume set
        meta_uri: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ConfigActions {
    /// Manage staging disk caches
    #[command(subcommand, alias = "diskcaches")]
    DiskCache(DiskCacheActions),
    /// Manage data volumes
    #[command(
        subcommand,
        alias = "datavolumes",
        alias = "datavolume",
        alias = "backends",
        alias = "backend",
        alias = "volumes"
    )]
    DataVolume(DataVolumeActions),
    /// Manage metadata volumes
    #[command(
        subcommand,
        alias = "metavolumes",
        alias = "metavolume",
        alias = "metadata_backends",
        alias = "metadata_backend"
    )]
    MetadataVolume(MetadataVolumeActions),
    /// Set runtime quotas (capacity, inodes, memory cache sizes)
    Set {
        /// Quota/config key
        ///
        /// One of "capacity", "inodes", "mem_cache_size",
        /// "read_mem_cache_size", "write_mem_cache_size", or
        /// "fuse_io_uring_sqpoll_idle_ms".
        key: String,
        /// New value (e.g. "100G", "2T" or numeric value/0)
        value: String,
    },
    /// Replace the staging/cache directories recorded at format
    ///
    /// Guarded like format: fails while any client has the volume
    /// mounted. The new directories are wiped so the next mount stamps
    /// a fresh staging generation into them. A directory that is not
    /// empty and was not previously used for squeezefs staging is only
    /// wiped when --force (or --yes) is passed; system paths such as
    /// /usr are never wiped.
    SetCachePaths {
        /// Metadata URI (sqmeta://...) of the filesystem to change
        uri: String,
        /// New staging/cache directory paths (replaces the recorded set)
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Confirm wiping a non-empty directory
        ///
        /// The deletion plan is printed before anything is removed.
        /// Without this flag, only empty, new, or previously used
        /// squeezefs staging directories are accepted.
        #[arg(long, short = 'f', alias = "yes")]
        force: bool,
    },
    /// Show the staging/cache directories recorded at format
    GetCachePaths {
        /// Metadata URI (sqmeta://...) of the filesystem to inspect
        uri: String,
    },
    /// List current configuration (diskcaches, volumes, active volume)
    List,
    /// Consistency check on metadata and block references
    Fsck,
}

#[derive(Subcommand, Debug, Clone)]
enum DataVolumeActions {
    /// Add a data volume
    Add {
        /// Volume ID
        volume_id: String,
        /// SqueezeFS LVM Volume path (e.g. "/dev/mapper/xai-xai02")
        #[arg(long, alias = "backing-dev")]
        volume: Option<String>,
        /// IP address of NVMe-oF target
        #[arg(long)]
        ip: Option<String>,
        /// Port of NVMe-oF target
        #[arg(long)]
        port: Option<u16>,
        /// Subsystem NQN of NVMe-oF target
        #[arg(long)]
        subnqn: Option<String>,
        /// Capacity (e.g. "100G"; default: the formatted volume capacity)
        #[arg(long)]
        capacity: Option<String>,
    },
    /// Remove a data volume
    Remove {
        /// Volume ID
        volume_id: String,
        /// Force removal ignoring safety checks
        #[arg(long)]
        force: bool,
    },
    /// List all data volumes and their status
    List,
    /// Enable a data volume
    Enable {
        /// Volume ID
        volume_id: String,
    },
    /// Disable a data volume
    Disable {
        /// Volume ID
        volume_id: String,
    },
    /// Migrate data from one data volume to another
    Migrate {
        /// Source volume ID/backing device path
        from_volume: String,
        /// Destination volume ID/backing device path
        to_volume: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum MetadataVolumeActions {
    /// Add a metadata volume
    Add {
        /// Volume ID
        volume_id: String,
        /// Backing device path
        #[arg(long, alias = "backing-dev")]
        volume: Option<String>,
        /// Optional capacity
        #[arg(long)]
        capacity: Option<String>,
    },
    /// Remove a metadata volume
    Remove {
        /// Volume ID
        volume_id: String,
        /// Force removal ignoring safety checks
        #[arg(long)]
        force: bool,
    },
    /// List all metadata volumes and their status
    List,
    /// Enable a metadata volume
    Enable {
        /// Volume ID (e.g., meta_volume_0 or index/path)
        volume_id: String,
    },
    /// Disable a metadata volume
    Disable {
        /// Volume ID (e.g., meta_volume_0 or index/path)
        volume_id: String,
    },
    /// Migrate inodes/metadata from one metadata volume to another
    Migrate {
        /// Source metadata volume index or ID
        from_volume: String,
        /// Destination metadata volume index or ID
        to_volume: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum DiskCacheActions {
    /// Add a staging disk cache path
    Add {
        /// Path to add
        path: String,
    },
    /// Remove a staging disk cache path
    Remove {
        /// Path to remove
        path: String,
        /// Force removal ignoring safety checks
        #[arg(long)]
        force: bool,
    },
    /// Enable a staging disk cache path
    Enable {
        /// Path to enable
        path: String,
    },
    /// Disable a staging disk cache path
    Disable {
        /// Path to disable
        path: String,
    },
    /// Flush a disabled staging disk cache path (drains staging writes)
    Flush {
        /// Path to flush
        path: String,
    },
    /// List all staging disk caches and their status
    List,
}

#[cfg(feature = "dhat-on")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(all(target_os = "linux", not(feature = "dhat-on")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc dirty-page decay, bounded (R1b liveness, measured): the hot
/// tier's 4 MiB `Bytes` insert→evict churn under a cold stream retains
/// freed-but-dirty pages for the default 10 s decay — at a ~4 GiB/s fill
/// rate that is multi-GiB of dead anon RSS charged to the cgroup (the
/// PR 4 row-2 8 GiB cage kill; pre-R4 the same allocations died in
/// microseconds and reused one arena chunk). 1 s decay returns dirty
/// pages fast enough for cage-sized deployments while keeping reuse
/// batching; muzzy stays 0 (default). R5's budget authority (PR 7)
/// subsumes this with real backpressure; the decay bound stays correct
/// regardless.
#[cfg(all(target_os = "linux", not(feature = "dhat-on")))]
#[export_name = "_rjem_malloc_conf"]
pub static MALLOC_CONF: &[u8] = b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0\0";

#[cfg(unix)]
static DAEMON_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

const FUSE_IO_URING_SQPOLL_IDLE_MS_ENV: &str = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS";
const FUSE_IO_URING_SQPOLL_CPU_ENV: &str = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU";

/// Resolve the mountpoint from the `mount` CLI shape: the first positional
/// when `--meta-lv` supplies the volumes, else the last. `None` when the
/// positional count is wrong — the existing arg-error paths own those
/// messages.
#[cfg(unix)]
fn mount_cli_mountpoint(args: &[String], meta_lv: &Option<Vec<String>>) -> Option<PathBuf> {
    if meta_lv.is_some() {
        args.first().map(PathBuf::from)
    } else if args.len() >= 2 {
        Some(PathBuf::from(&args[args.len() - 1]))
    } else {
        None
    }
}

/// PARENT-side mountpoint preflight: the daemon's own refusal conditions
/// (fuse3 `Session::mount_empty_check` refuses a non-empty mountpoint
/// unconditionally — `MountOptions::nonempty` is never set — and
/// `start_mount` refuses a stale ENOTCONN/EIO attachment), hoisted so they
/// fail BEFORE daemonizing and before any volume is touched. Without this
/// the child refused the mountpoint after the fork and the parent could
/// only report a generic handshake failure (the live-diagnosed
/// "not ready in 30 seconds" while `$MNT/stray` was the actual cause).
#[cfg(unix)]
fn validate_mountpoint_preflight(mountpoint: &Path) -> Result<(), String> {
    let meta = match std::fs::metadata(mountpoint) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "mountpoint '{}' does not exist",
                mountpoint.display()
            ));
        }
        Err(e) => {
            let os = e.raw_os_error();
            if os == Some(libc::ENOTCONN) || os == Some(libc::EIO) {
                return Err(format!(
                    "mountpoint '{}' is a stale FUSE mount ({}); run `squeezefs umount {}` \
                     (or `umount -l`) first",
                    mountpoint.display(),
                    e,
                    mountpoint.display()
                ));
            }
            return Err(format!(
                "cannot access mountpoint '{}': {}",
                mountpoint.display(),
                e
            ));
        }
    };
    if !meta.is_dir() {
        return Err(format!(
            "mountpoint '{}' is not a directory",
            mountpoint.display()
        ));
    }
    // Mirror of the daemon's empty check: name the first stray entry so
    // the operator knows exactly what blocks the mount.
    let mut entries = std::fs::read_dir(mountpoint)
        .map_err(|e| format!("cannot read mountpoint '{}': {}", mountpoint.display(), e))?;
    if let Some(first) = entries.next() {
        let name = first
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<unreadable entry>".to_string());
        return Err(format!(
            "mountpoint '{}' is not empty (found '{}'); refusing to mount",
            mountpoint.display(),
            name
        ));
    }
    Ok(())
}

/// Fail the mount bootstrap loud. On a daemonized child (stdout/stderr
/// already point at /dev/null or the log file) the message is ALSO written
/// to the parent's handshake pipe, so the parent surfaces the actual
/// reason instead of a generic "child exited early".
#[cfg(unix)]
fn mount_bootstrap_fail(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    let fd = DAEMON_PIPE.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        let line = format!("Error: {msg}\n");
        let _ = unsafe { libc::write(fd, line.as_ptr() as *const libc::c_void, line.len()) };
        let _ = unsafe { libc::close(fd) };
        DAEMON_PIPE.store(-1, std::sync::atomic::Ordering::Relaxed);
    }
    std::process::exit(1);
}

/// Bounded tail of the daemon log (the parent's last diagnostic when a
/// child died without a pipe message — e.g. SIGKILL'd by the OOM killer).
#[cfg(unix)]
fn print_daemon_log_tail(path: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL_BYTES: u64 = 4096;
    const TAIL_LINES: usize = 20;
    let Ok(mut f) = std::fs::File::open(path) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return;
    }
    if f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .is_err()
    {
        return;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return;
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let shown = &lines[lines.len().saturating_sub(TAIL_LINES)..];
    if shown.iter().all(|l| l.trim().is_empty()) {
        return;
    }
    eprintln!("--- daemon log tail ({}) ---", path.display());
    for line in shown {
        eprintln!("{line}");
    }
}

fn resolve_local_sqpoll_cpu(explicit_override: Option<u32>) -> Option<u32> {
    explicit_override.filter(|value| *value > 0)
}

#[cfg(target_os = "linux")]
fn apply_fuse_io_uring_sqpoll_env(idle_ms: Option<u32>, cpu: Option<u32>) {
    if let Some(idle_ms) = idle_ms {
        std::env::set_var(FUSE_IO_URING_SQPOLL_IDLE_MS_ENV, idle_ms.to_string());
    } else {
        std::env::remove_var(FUSE_IO_URING_SQPOLL_IDLE_MS_ENV);
    }

    if let Some(cpu) = cpu {
        std::env::set_var(FUSE_IO_URING_SQPOLL_CPU_ENV, cpu.to_string());
    } else {
        std::env::remove_var(FUSE_IO_URING_SQPOLL_CPU_ENV);
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_fuse_io_uring_sqpoll_env(_idle_ms: Option<u32>, _cpu: Option<u32>) {}

fn parse_block_uri(uri: &str, scheme: &str) -> Result<Vec<String>, String> {
    if !uri.starts_with(scheme) {
        return Err(format!(
            "URI must start with scheme '{}' (got '{}')",
            scheme, uri
        ));
    }
    let rest = &uri[scheme.len()..];
    let clean = rest.trim_start_matches('/');
    if clean.is_empty() {
        return Err(format!("Empty paths in URI '{}'", uri));
    }
    let paths: Vec<String> = clean
        .split(',')
        .map(|p| {
            if p.starts_with('/') {
                p.to_string()
            } else {
                format!("/{}", p)
            }
        })
        .collect();
    Ok(paths)
}

/// VL2 admin-lane client: discover the mount's ctl socket from the
/// bootstrap xattr on the mountpoint root (armed on every mount since
/// VL2), open an ADMIN session (peercred-gated daemon-side), run one
/// verb.
fn admin_roundtrip(mountpoint: &str, verb: &str, arg: &str) -> Result<String, String> {
    // Read the bootstrap blob via plain getxattr on the mountpoint.
    let cpath = std::ffi::CString::new(mountpoint).map_err(|_| "bad path".to_string())?;
    let name = std::ffi::CString::new(squeezefs_ipc::wire::BOOTSTRAP_XATTR).unwrap();
    let mut buf = vec![0u8; 4096];
    // SAFETY: getxattr(2) into our sized buffer.
    let n = unsafe {
        libc::getxattr(
            cpath.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if n < 0 {
        return Err(format!(
            "{mountpoint} does not answer the SqueezeFS bootstrap probe — not a live \
             SqueezeFS mount? ({})",
            std::io::Error::last_os_error()
        ));
    }
    let blob = squeezefs_ipc::wire::BootstrapBlob::decode(&buf[..n as usize])
        .map_err(|e| format!("bootstrap blob undecodable: {e:?}"))?;
    // Connect ladder (OQ-6): abstract first, path fallback.
    let sock = squeezefs::ipc_host::abstract_connect(&blob.socket)
        .or_else(|e| {
            if blob.socket_path.is_empty() {
                Err(e)
            } else {
                squeezefs::ipc_host::path_connect(&blob.socket_path)
            }
        })
        .map_err(|e| format!("cannot reach the mount's ctl socket: {e}"))?;
    use squeezefs::ipc_host::{recv_ctl, send_ctl};
    use squeezefs_ipc::wire::CtlMsg;
    // SAFETY: getpid/getuid are trivially safe.
    let (pid, uid) = unsafe { (libc::getpid() as u32, libc::getuid()) };
    send_ctl(&sock, &CtlMsg::AdminHello { pid, uid }, None).map_err(|e| e.to_string())?;
    match recv_ctl(&sock).map_err(|e| e.to_string())?.0 {
        CtlMsg::AdminOk => {}
        CtlMsg::Refuse { class } => {
            return Err(format!(
                "admin session refused ({class:?}) — the lane admits root or the mount-owning uid"
            ))
        }
        other => return Err(format!("unexpected reply {other:?}")),
    }
    send_ctl(
        &sock,
        &CtlMsg::AdminReq {
            verb: verb.to_string(),
            arg: arg.to_string(),
        },
        None,
    )
    .map_err(|e| e.to_string())?;
    match recv_ctl(&sock).map_err(|e| e.to_string())?.0 {
        CtlMsg::AdminReply { ok: true, body } => Ok(body),
        CtlMsg::AdminReply { ok: false, body } => Err(body),
        other => Err(format!("unexpected reply {other:?}")),
    }
}

/// PR VL2b: `squeezefs job worker <sqmeta-uri>` — enroll this client as
/// a remote data-plane worker over the §5.1.6 wire. Discovery and the
/// enrollment secret both ride read-only probe opens (the clients/df
/// access pattern — no D0 claim); the coordinator is dialed over
/// TCP (the sanctioned non-uring network path).
async fn run_job_worker(meta_uri: &str) -> Result<(), Box<dyn std::error::Error>> {
    let meta_lvs = parse_block_uri(meta_uri, "sqmeta://")?;
    // Routed probe (PR VL5a): stamped sets canonicalize + route with
    // their frozen width/slot map; legacy sets keep URI order.
    let routed = squeezefs::meta_backend::open_probe_routed_meta_set(&meta_lvs).await?;
    let secret = squeezefs::job_wire::read_enroll_secret(&routed)
        .await
        .map_err(|e| format!("storage-membership credential unavailable: {e}"))?;
    let endpoint = squeezefs::job_wire::discover_endpoint(&routed)
        .await
        .ok_or(
            "no live coordinator publishes a job_endpoint in the mount registrations — \
             is a (post-VL2b) writer mounted on this volume set?",
        )?;
    println!("job worker: coordinator endpoint {endpoint} (discovered from mount registrations)");
    // Guarantee-class detection (§5.1.6 startup-log requirement): this
    // verb registers no per-host PR key on the data namespaces yet, so
    // THIS worker host's zombie fence is the deferred-reclaim class
    // (rung 3 documented residual); coordinators still PR-preempt hosts
    // that do report a pr_key where RESCAP supports it.
    println!(
        "job worker: guarantee class deferred-reclaim for this host (no per-host \
         data-namespace PR registration in this verb; design-volume-lifecycle §5.1.6 rung 3)"
    );
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown-host".to_string());
    let worker_id = format!("{hostname}:{}", std::process::id());
    let worker = squeezefs::job_wire::JobWireWorker::connect(
        &endpoint,
        &secret,
        squeezefs::job_wire::WorkerOptions::new(&worker_id),
    )
    .await
    .map_err(|e| format!("enrollment failed: {e}"))?;
    println!("job worker: enrolled as {worker_id}; serving shards (stop with SIGINT)");
    let report = worker
        .run(std::sync::Arc::new(squeezefs::job_wire::NoopDeviceSeam))
        .await?;
    println!(
        "job worker: coordinator connection closed — shards completed {}, submissions \
         refused {}, aborted by lease re-validation {}",
        report.shards_completed, report.submissions_refused, report.shards_aborted
    );
    Ok(())
}

/// PR VL6a: the `squeezefs fsck` / `squeezefs scrub` verb bodies
/// (design-volume-lifecycle §5.6/§6). Online = submit the report-only
/// detection job over the admin lane and poll to completion; offline =
/// read-only probes with optional `--shards k/N` zero-coordination
/// sharding; `fsck merge-reports <files…>` unions shard outputs.
/// Detection never mutates; the process exits nonzero when verified
/// findings exist (the fsck convention).
/// PR VL6b: the `--repair [--apply] [--quarantine-dir]` CLI surface.
struct FsckRepairArgs {
    repair: bool,
    apply: bool,
    quarantine_dir: Option<String>,
}

#[allow(clippy::too_many_arguments)] // a CLI verb surface, not an API
async fn run_fsck_verb(
    target: &str,
    reports: &[String],
    force_online: bool,
    force_offline: bool,
    throttle: u32,
    json: bool,
    shards: Option<String>,
    scrub: bool,
    scrub_only: bool,
    repair_args: FsckRepairArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    squeezefs::set_fs_prefix("squeezefs");

    // §5.6a argument lattice: dry-run is the DEFAULT (--repair plans;
    // --repair --apply executes); apply/quarantine-dir without --repair
    // are refused loud; --repair on probe shards is refused (repair
    // needs whole-scan findings under exclusive/coordinator authority).
    if (repair_args.apply || repair_args.quarantine_dir.is_some()) && !repair_args.repair {
        return Err(
            "--apply/--quarantine-dir are only valid with --repair (§5.6a: repair is \
             dry-run by default)"
                .into(),
        );
    }
    if repair_args.repair && shards.is_some() {
        return Err(
            "--repair is refused on --shards probe shards: repair requires the whole-scan \
             findings under the guarded open (§5.6a); run the repair unsharded"
                .into(),
        );
    }

    // `fsck merge-reports <files…>` — the shard union.
    if target == "merge-reports" {
        if reports.is_empty() {
            return Err("usage: squeezefs fsck merge-reports <report.json…>".into());
        }
        let mut parsed = Vec::with_capacity(reports.len());
        for path in reports {
            let bytes = std::fs::read(path)
                .map_err(|e| format!("cannot read shard report '{path}': {e}"))?;
            parsed.push(
                serde_json::from_slice::<squeezefs::fsck::FsckReport>(&bytes)
                    .map_err(|e| format!("'{path}' is not an fsck report: {e}"))?,
            );
        }
        let merged = squeezefs::fsck::merge_reports(&parsed);
        print_fsck_report(&merged, json);
        if merged.has_findings() {
            std::process::exit(1);
        }
        return Ok(());
    }
    if !reports.is_empty() {
        return Err("trailing report files are only valid with `fsck merge-reports`".into());
    }

    let live = !target.starts_with("sqmeta://");
    if force_offline && live {
        return Err(
            "--offline needs the sqmeta:// URI (offline mode is read-only probes with \
             nothing in flight; a mountpoint target runs ONLINE against the live daemon)"
                .into(),
        );
    }
    if force_online && !live {
        return Err(
            "--online needs a live mountpoint target (the online scan reads the daemon's \
             RAM-authoritative state over the admin lane)"
                .into(),
        );
    }

    if live {
        if shards.is_some() {
            return Err("--shards k/N is the OFFLINE zero-coordination mode \
                        (design-volume-lifecycle §5.6); online fsck runs whole on the \
                        coordinator"
                .into());
        }
        let mut arg = String::new();
        if scrub_only {
            arg.push_str("scrub-only ");
        } else if scrub {
            arg.push_str("scrub ");
        }
        if repair_args.repair {
            arg.push_str("repair ");
            if repair_args.apply {
                arg.push_str("apply ");
            }
            if let Some(qdir) = &repair_args.quarantine_dir {
                arg.push_str(&format!("qdir {qdir} "));
            }
        }
        arg.push_str(&format!("throttle {throttle}"));
        let body = admin_roundtrip(target, "fsck", arg.trim())?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("undecodable admin reply: {e}"))?;
        let job_id = v["job_id"]
            .as_str()
            .ok_or("admin reply carried no job id")?
            .to_string();
        eprintln!("fsck job {job_id} running (report-only; watch `squeezefs job list`)…");
        loop {
            let st = admin_roundtrip(target, "job-status", &job_id)?;
            let sv: serde_json::Value =
                serde_json::from_str(&st).map_err(|e| format!("undecodable job status: {e}"))?;
            match sv["state"].as_str().unwrap_or("?") {
                "completed" => break,
                "failed" => return Err(format!("fsck job {job_id} failed (see daemon log)").into()),
                "cancelled" => return Err(format!("fsck job {job_id} was cancelled").into()),
                "paused" | "paused-capacity" => {
                    return Err(format!(
                        "fsck job {job_id} was paused — `squeezefs job resume` re-queues it \
                         (detection re-runs whole)"
                    )
                    .into())
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
            }
        }
        let body = admin_roundtrip(target, "fsck-report", &job_id)?;
        let report: squeezefs::fsck::FsckReport =
            serde_json::from_str(&body).map_err(|e| format!("undecodable fsck report: {e}"))?;
        print_fsck_report(&report, json);
        if report.has_findings() {
            std::process::exit(1);
        }
        return Ok(());
    }

    // Offline: read-only probes (refused loud under a live writer).
    let meta_lvs = parse_block_uri(target, "sqmeta://")?;
    let shard = match &shards {
        None => None,
        Some(spec) => {
            let (k, n) = spec
                .split_once('/')
                .and_then(|(k, n)| Some((k.parse::<u32>().ok()?, n.parse::<u32>().ok()?)))
                .filter(|(k, n)| *n > 0 && k < n)
                .ok_or("--shards expects k/N with 0 <= k < N (e.g. 0/4)")?;
            Some((k, n))
        }
    };
    let mut opts = squeezefs::fsck::FsckOptions::offline();
    opts.throttle_pct = throttle;
    opts.shard = shard;
    opts.scrub = scrub;
    opts.scrub_only = scrub_only;
    let report = if repair_args.repair {
        // §5.6a: offline repair is a WRITER — the guarded D0 open, never
        // the read-only probe (run_offline_repair enforces it).
        let ropts = squeezefs::fsck::RepairOptions {
            apply: repair_args.apply,
            quarantine_dir: repair_args.quarantine_dir.clone().map(Into::into),
        };
        squeezefs::fsck::run_offline_repair(&meta_lvs, &opts, &ropts).await?
    } else {
        squeezefs::fsck::run_offline(&meta_lvs, &opts).await?
    };
    print_fsck_report(&report, json);
    if report.has_findings() {
        std::process::exit(1);
    }
    Ok(())
}

fn print_fsck_report(report: &squeezefs::fsck::FsckReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).expect("report serializes")
        );
        return;
    }
    let c = &report.counters;
    println!(
        "fsck ({} mode{}): {} inode(s), {} tree page(s), {} block(s), {} refcount(s) checked \
         in {} s",
        report.mode,
        report
            .shard
            .as_deref()
            .map(|s| format!(", shard {s}"))
            .unwrap_or_default(),
        c.inodes_scanned,
        c.nodes_walked,
        c.blocks_checked,
        c.refcounts_checked,
        c.scan_secs,
    );
    println!(
        "suspects: {} raised, {} cleared ({} epoch-exempt, {} in-flight-exempt, \
         {} mover-ledger-exempt)",
        c.suspects,
        c.suspects_cleared,
        c.epoch_exempted,
        c.inflight_exempted,
        c.mover_ledger_exempted,
    );
    if c.scrub_blocks_scanned > 0 {
        println!(
            "scrub: {} block(s) / {} B — {} AEAD-verified, {} frame-verified, \
             {} readability-only (no stored checksum — OQ-B), {} failure(s)",
            c.scrub_blocks_scanned,
            c.scrub_bytes_scanned,
            c.scrub_aead_verified,
            c.scrub_frame_verified,
            c.scrub_readability_only,
            c.scrub_failures,
        );
    }
    if report.findings.is_empty() {
        println!("findings: 0 (clean)");
    } else {
        println!("findings: {}:", report.findings.len());
        for f in &report.findings {
            println!("  [{}] {} — {}", f.class, f.object, f.evidence);
        }
    }
    if let Some(rep) = &report.repair {
        println!(
            "repair ({}): {} planned, {} applied, {} refused; quarantined {} record(s) \
             + {} block(s) = {} B{}",
            if rep.dry_run {
                "DRY RUN — pass --apply to execute"
            } else {
                "applied"
            },
            rep.counters.planned,
            rep.counters.applied,
            rep.counters.refused,
            rep.counters.quarantined_records,
            rep.counters.quarantined_blocks,
            rep.counters.quarantined_bytes,
            rep.quarantine_dir
                .as_deref()
                .map(|d| format!(" (quarantine: {d})"))
                .unwrap_or_default(),
        );
        for a in &rep.planned {
            println!(
                "  plan  [{}] {} — {}: {}",
                a.class, a.object, a.action, a.detail
            );
        }
        for a in &rep.applied {
            println!(
                "  done  [{}] {} — {}: {}",
                a.class, a.object, a.action, a.detail
            );
        }
        for a in &rep.refused {
            println!("  skip  [{}] {} — {}", a.class, a.object, a.detail);
        }
    }
}

/// The `squeezefs defrag` verb's flag lattice (PR VL7, §5.7).
struct DefragVerbArgs {
    data: bool,
    volume: Option<String>,
    meta: bool,
    fold: bool,
    rebalance: bool,
    report_only: bool,
    throttle: u32,
    json: bool,
}

fn print_defrag_report(report: &squeezefs::defrag::DefragReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).expect("report serializes")
        );
        return;
    }
    println!("defrag report (four-axis model, design-volume-lifecycle §5.7 / KD-11):");
    for r in &report.d1 {
        println!(
            "  D1 {}: contiguity {:.3}, reclaimable tail {:.3} ({} free / {} used of {} \
             blocks, largest run {})",
            r.id,
            r.contiguity,
            r.reclaimable_tail,
            r.free_blocks,
            r.used_blocks,
            r.space_blocks,
            r.largest_free_run,
        );
    }
    println!(
        "  D2 locality {:.3} ({} local of {} adjacent pairs over {} file(s))",
        report.d2.locality, report.d2.local_pairs, report.d2.pairs, report.d2.files
    );
    println!(
        "  D3 pressure {} B ({} parked + {} spilled across {} record(s))",
        report.d3.pressure_bytes,
        report.d3.parked_extent_bytes,
        report.d3.spilled_record_bytes,
        report.d3.spilled_records
    );
    for r in &report.d4 {
        println!(
            "  D4 {}: dead-bset ratio {:.3} ({} live of {} records over {} leaves)",
            r.volume, r.dead_bset_ratio, r.records_live, r.records_indexed, r.leaves
        );
    }
}

/// Poll a submitted job to terminal over the admin lane (the fsck verb's
/// wait shape).
fn admin_wait_job_terminal(target: &str, job_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let st = admin_roundtrip(target, "job-status", job_id)?;
        let sv: serde_json::Value =
            serde_json::from_str(&st).map_err(|e| format!("undecodable job status: {e}"))?;
        match sv["state"].as_str().unwrap_or("?") {
            "completed" => return Ok(()),
            "failed" => return Err(format!("defrag job {job_id} failed (see daemon log)").into()),
            "cancelled" => return Err(format!("defrag job {job_id} was cancelled").into()),
            "paused" | "paused-capacity" => {
                return Err(format!(
                    "defrag job {job_id} was paused — `squeezefs job resume` re-queues it"
                )
                .into())
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
}

async fn run_defrag_verb(
    target: &str,
    args: DefragVerbArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    squeezefs::set_fs_prefix("squeezefs");
    let modes = [
        args.data,
        args.meta,
        args.fold,
        args.rebalance,
        args.report_only,
    ]
    .iter()
    .filter(|m| **m)
    .count();
    if modes != 1 {
        return Err(
            "pick exactly one of --data / --meta / --fold / --rebalance / --report-only \
             (each §5.7 axis is independently invocable)"
                .into(),
        );
    }
    if args.volume.is_some() && !args.data {
        return Err("--volume only scopes --data (D1/D2 are the per-volume movers)".into());
    }
    let live = !target.starts_with("sqmeta://");

    if live {
        if args.report_only {
            let body = admin_roundtrip(target, "defrag", "report")?;
            let report: squeezefs::defrag::DefragReport =
                serde_json::from_str(&body).map_err(|e| format!("undecodable report: {e}"))?;
            print_defrag_report(&report, args.json);
            return Ok(());
        }
        let mode = if args.data {
            "data"
        } else if args.meta {
            "meta"
        } else if args.fold {
            "fold"
        } else {
            "rebalance"
        };
        let mut arg = mode.to_string();
        if let Some(v) = &args.volume {
            arg.push_str(&format!(" vol {v}"));
        }
        arg.push_str(&format!(" throttle {}", args.throttle));
        let body = admin_roundtrip(target, "defrag", &arg)?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("undecodable admin reply: {e}"))?;
        let job_id = v["job_id"]
            .as_str()
            .ok_or("admin reply carried no job id")?
            .to_string();
        eprintln!(
            "defrag --{mode} job {job_id} running at {} % throttle (watch \
             `squeezefs job list {target}`)…",
            args.throttle
        );
        admin_wait_job_terminal(target, &job_id)?;
        println!("defrag --{mode} completed (job {job_id}).");
        return Ok(());
    }

    // Offline (§5.8): report-only = read-only probes; movers = the
    // short-lived D0-guarded coordinator running the job in-process.
    let meta_lvs = parse_block_uri(target, "sqmeta://")?;
    if args.report_only {
        let report = squeezefs::defrag::run_offline_report(&meta_lvs).await?;
        print_defrag_report(&report, args.json);
        return Ok(());
    }
    if args.fold {
        return Err(
            "defrag --fold is LIVE-ONLY: fold custody (parked overlays + staged \
             extent records) is mount-owned — run `squeezefs defrag <mountpoint> \
             --fold`; an unmounted set's records drain at the next mount's recovery"
                .into(),
        );
    }
    let job_type = if args.data {
        squeezefs::jobs::JobType::DefragData {
            volume_id: args.volume.clone(),
        }
    } else if args.meta {
        squeezefs::jobs::JobType::DefragMeta
    } else {
        squeezefs::jobs::JobType::Rebalance
    };
    squeezefs::defrag::run_offline_mover(&meta_lvs, job_type, args.throttle).await?;
    println!("offline defrag completed.");
    Ok(())
}

/// Offline probe: read the durable job records straight off the meta
/// volumes (the clients/df access pattern — read-only, no D0 claim).
async fn job_probe_records(
    uri: &str,
) -> Result<Vec<squeezefs::jobs::JobRecord>, Box<dyn std::error::Error>> {
    let meta_lvs = parse_block_uri(uri, "sqmeta://")?;
    // Routed probe (PR VL5a): canonical order + frozen width for
    // stamped sets; byte-identical legacy behavior otherwise.
    let routed = squeezefs::meta_backend::open_probe_routed_meta_set(&meta_lvs).await?;
    Ok(squeezefs::jobs::JobFabric::list_records(&routed).await?)
}

/// Where a `config` volume-family verb acts (PR VL3 — the `/dev/shm`
/// runtime-config file is gone): `-g sqmeta://…` targets the DURABLE
/// records offline (guarded); `-g <mountpoint>` or the single live mount
/// targets the daemon's admin lane.
enum ConfigTarget {
    Offline(Vec<String>),
    Live(String),
}

fn resolve_config_target(
    meta_uri: &Option<String>,
) -> Result<ConfigTarget, Box<dyn std::error::Error>> {
    if let Some(uri) = meta_uri {
        if uri.starts_with("sqmeta://") {
            return Ok(ConfigTarget::Offline(parse_block_uri(uri, "sqmeta://")?));
        }
        return Ok(ConfigTarget::Live(uri.clone()));
    }
    let mounts = find_squeezefs_mounts();
    match mounts.len() {
        0 => Err(
            "no live SqueezeFS mount found; pass -g sqmeta://<volumes> for the \
                  offline durable path, or -g <mountpoint> for a live daemon"
                .into(),
        ),
        1 => Ok(ConfigTarget::Live(mounts[0].display().to_string())),
        n => Err(
            format!("{n} live SqueezeFS mounts found — pass -g <mountpoint> to pick one").into(),
        ),
    }
}

/// VL1 (design-volume-lifecycle §5.0): the removed fake admin verbs
/// refuse loudly — exit nonzero, naming what was fake and the successor
/// verb — never stub success.
fn removed_verb(verb: &str, why: &str, successor: &str) -> Box<dyn std::error::Error> {
    format!(
        "`squeezefs {verb}` was removed: {why} \
         Superseded by `{successor}` (docs/design-volume-lifecycle.md)."
    )
    .into()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "dhat-on")]
    let _profiler = dhat::Profiler::new_heap();

    // ENG-10: the ONE env-knob convention's enforcement point. Before any
    // argument is interpreted, any volume opened or anything mounted: every
    // SQUEEZEFS_*/SQZ_* variable present is validated against the registry
    // (`src/env_knobs.rs`). A malformed value, an out-of-range value or a
    // retired spelling refuses the process here, naming EVERY offender at
    // once — the 56 silent-default sites downstream can no longer turn a
    // typo into "the knob did nothing". Unregistered names are announced as
    // probable typos (never refused: a mixed-version fleet and the shim's
    // client-side knobs share this environment).
    if let Some(report) = squeezefs::env_knobs::refusal_report() {
        eprintln!("Error: {report}");
        std::process::exit(1);
    }

    let mut cli = Cli::parse();

    // ENG-3 (the daemon must be audible): the --log-file target must be
    // openable BEFORE anything else runs. In daemon mode stdio is
    // redirected to /dev/null, so a swallowed open error means a daemon
    // running with ALL logging discarded. Checked here in the parent,
    // pre-fork: the refusal is loud on the caller's console for
    // foreground and --daemon alike, and nothing is mounted.
    if let Some(ref log_path) = cli.log_file {
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            eprintln!("Error: cannot open --log-file {}: {e}", log_path.display());
            std::process::exit(1);
        }
    }

    // Cache-path policy: staging/cache directories are DECLARED AT FORMAT
    // and recorded in the format config — the single source of truth. A
    // mount-time override is exactly how the stale-staging poisoning
    // incident happened (mount conjured/reused caches format never
    // declared), so it is a LOUD error, never a silent ignore. Checked
    // before daemonizing and before any volume is touched: instant.
    if let Commands::Mount {
        disk_cache_paths: Some(_),
        ..
    } = &cli.command
    {
        eprintln!(
            "Error: cache paths are fixed at format; use `squeezefs config set-cache-paths` \
             to change them (mount does not accept --disk-cache-paths)"
        );
        std::process::exit(1);
    }

    // Mountpoint preflight in the PARENT, before daemonizing and before any
    // volume is touched: the daemon's refusal conditions (exists / is a
    // directory / is empty / not a stale attachment) fail instantly with a
    // precise error on the caller's console — never a generic handshake
    // timeout after the child refused post-fork.
    #[cfg(unix)]
    if let Commands::Mount {
        ref args,
        ref meta_lv,
        ..
    } = &cli.command
    {
        if let Some(mountpoint) = mount_cli_mountpoint(args, meta_lv) {
            if let Err(msg) = validate_mountpoint_preflight(&mountpoint) {
                eprintln!("Error: {msg}");
                std::process::exit(1);
            }
        }
    }

    // VAL-3 (docs/design-key-handling.md §3/§4): read the encryption key
    // material HERE — in the parent, before `--daemon` forks — so `-`
    // (stdin) works for daemonized mounts and so a bad key file fails on
    // the caller's console instead of over the handshake pipe. What
    // crosses the fork is the material in memory, never argv, never the
    // volume. The parent clears its copy the moment the child owns one.
    #[cfg(unix)]
    if let Commands::Mount {
        encrypt_key: Some(spec),
        ..
    } = &cli.command
    {
        match squeezefs::keyfile::read_key_source(spec) {
            Ok(material) => squeezefs::keyfile::stash_key_material(material),
            Err(msg) => {
                eprintln!("Error: {msg}");
                std::process::exit(1);
            }
        }
    }

    #[cfg(unix)]
    if let Commands::Mount {
        ref args,
        daemon: true,
        supervise,
        ref meta_lv,
        ..
    } = &cli.command
    {
        let supervise = *supervise;
        // Resolve mountpoint just for the parent process printing/waiting
        let mountpoint_path = if let Some(ref _m_lvs) = meta_lv {
            if args.is_empty() {
                eprintln!("Error: Mountpoint path is required");
                std::process::exit(1);
            }
            PathBuf::from(&args[0])
        } else {
            if args.len() < 2 {
                eprintln!("Error: Metadata URI (sqmeta://...) and Mountpoint path are required");
                std::process::exit(1);
            }
            PathBuf::from(&args[args.len() - 1])
        };

        let mut pipefd = [0; 2];
        unsafe {
            if libc::pipe(pipefd.as_mut_ptr()) < 0 {
                eprintln!("Failed to create daemon pipe");
                std::process::exit(1);
            }
            let pid = libc::fork();
            if pid < 0 {
                eprintln!("Failed to fork daemon process");
                std::process::exit(1);
            } else if pid > 0 {
                // Parent process: wait for the child's handshake — "ready\n"
                // (mount armed), an "Error:"/"Panic:" line (child refused —
                // surface its ACTUAL reason), EOF (child gone), or the 30 s
                // deadline. The pre-fix handshake discarded collected error
                // content and printed a bogus "not ready in 30 seconds"
                // whenever the child had not been reaped yet (the
                // live-diagnosed non-empty-mountpoint symptom).
                libc::close(pipefd[1]);

                // VAL-3/VAL-7h: the child owns the key material now; the
                // waiting parent must not keep a copy alive.
                squeezefs::keyfile::clear_key_material();

                let mut child_output = String::new();
                let mut ready = false;
                let mut child_failed = false;
                let start = std::time::Instant::now();

                print!("Mounting Squeezefs at {:?}...", mountpoint_path);
                use std::io::Write;
                let _ = std::io::stdout().flush();

                let mut pfd = libc::pollfd {
                    fd: pipefd[0],
                    events: libc::POLLIN,
                    revents: 0,
                };

                loop {
                    let elapsed = start.elapsed().as_millis() as i32;
                    let timeout = (30000 - elapsed).max(0);
                    if timeout == 0 {
                        break;
                    }
                    let r = libc::poll(&mut pfd, 1, timeout);
                    if r < 0 {
                        let e = std::io::Error::last_os_error();
                        if e.raw_os_error() == Some(libc::EINTR) {
                            continue;
                        }
                        break;
                    }
                    if r == 0 {
                        break;
                    }
                    let mut buf = [0u8; 1024];
                    let n = libc::read(pipefd[0], buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                    if n < 0 {
                        let e = std::io::Error::last_os_error();
                        if e.raw_os_error() == Some(libc::EINTR) {
                            continue;
                        }
                        break;
                    }
                    if n == 0 {
                        break;
                    }
                    child_output.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
                    if child_output.contains("ready\n") {
                        ready = true;
                        break;
                    }
                    // A complete failure line ends the wait immediately —
                    // the child is exiting, not becoming ready.
                    if (child_output.contains("Error:") || child_output.contains("Panic:"))
                        && child_output.ends_with('\n')
                    {
                        child_failed = true;
                        break;
                    }
                }

                println!();
                if ready {
                    println!(
                        "\x1b[92mOK\x1b[0m Squeezefs is ready at {:?}",
                        mountpoint_path
                    );
                    libc::close(pipefd[0]);
                    if supervise {
                        // Survey P1-B (JuiceFS cmd/mount_unix.go precedent):
                        // the parent stays alive as the external watchdog.
                        // It knows the daemon PID from its own fork —
                        // kill-by-PID discipline for free. std-only loop
                        // (the parent never starts a tokio runtime).
                        let env_secs = |key: &str, default: u64| {
                            std::env::var(key)
                                .ok()
                                .and_then(|v| v.parse::<u64>().ok())
                                .filter(|&s| s > 0)
                                .unwrap_or(default)
                        };
                        let policy = squeezefs::supervisor::SupervisorPolicy {
                            probe_interval: std::time::Duration::from_secs(env_secs(
                                "SQUEEZEFS_SUPERVISE_INTERVAL_SECS",
                                5,
                            )),
                            unresponsive_after: std::time::Duration::from_secs(env_secs(
                                "SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS",
                                30,
                            )),
                            write_abort: true,
                        };
                        squeezefs::supervisor::run_supervisor(
                            &mountpoint_path,
                            pid as u32,
                            policy,
                            std::path::Path::new(squeezefs::supervisor::FUSE_CONNECTIONS_SYSFS),
                        );
                        std::process::exit(0);
                    }
                    std::process::exit(0);
                }

                // Not ready. Reap the child with a bounded grace so a child
                // that already reported failure (or closed the pipe on its
                // way out) is never misreported as a 30 s timeout.
                let mut status = 0;
                let mut child_exited = false;
                let reap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let r = libc::waitpid(pid, &mut status, libc::WNOHANG);
                    if r == pid || r < 0 {
                        child_exited = true;
                        break;
                    }
                    if std::time::Instant::now() >= reap_deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if !child_exited {
                    // Genuine wedge: detach the mountpoint and kill the child.
                    let _ = std::process::Command::new("umount")
                        .arg("-l")
                        .arg(&mountpoint_path)
                        .output();
                    libc::kill(pid, libc::SIGKILL);
                    let _ = libc::waitpid(pid, &mut status, 0);
                }

                // Drain whatever the child managed to write before it died
                // (non-blocking: the writer may be gone or SIGKILL'd).
                let flags = libc::fcntl(pipefd[0], libc::F_GETFL);
                if flags >= 0 {
                    let _ = libc::fcntl(pipefd[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                let mut buf = [0u8; 1024];
                loop {
                    let n = libc::read(pipefd[0], buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                    if n <= 0 {
                        break;
                    }
                    child_output.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
                }
                libc::close(pipefd[0]);

                let reason = child_output.replace("ready\n", "");
                let reason = reason.trim();
                if !reason.is_empty() {
                    eprintln!("Failed to start squeezefs daemon:\n{}", reason);
                } else if child_failed || child_exited {
                    eprintln!(
                        "Failed to start squeezefs daemon: the daemon exited before the mount \
                         became ready (no error reported over the handshake pipe)."
                    );
                } else {
                    eprintln!(
                        "Error: mount did not become ready within 30 seconds; the daemon was \
                         killed and {:?} lazily detached.",
                        mountpoint_path
                    );
                }
                if let Some(ref log_path) = cli.log_file {
                    print_daemon_log_tail(log_path);
                }
                std::process::exit(1);
            }

            // Child process continues here
            libc::close(pipefd[0]);
            DAEMON_PIPE.store(pipefd[1], std::sync::atomic::Ordering::Relaxed);
            std::env::set_var("SQUEEZEFS_DAEMON_PIPE", pipefd[1].to_string());

            std::panic::set_hook(Box::new(|panic_info| {
                let fd = DAEMON_PIPE.load(std::sync::atomic::Ordering::Relaxed);
                if fd >= 0 {
                    let msg = format!("Panic: {}\n", panic_info);
                    let _ = libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len());
                    let _ = libc::close(fd);
                }
                eprintln!("PANIC occurred: {}", panic_info);
            }));

            libc::setsid();
            let _ = std::env::set_current_dir("/");
            if let Ok(null_file) = std::fs::File::open("/dev/null") {
                use std::os::unix::io::AsRawFd;
                libc::dup2(null_file.as_raw_fd(), 0);
            }
            let output_file = if let Some(ref path) = cli.log_file {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    Ok(f) => Some(f),
                    Err(e) => {
                        // ENG-3: the parent preflighted this open; a
                        // post-fork failure (target unlinked/permission
                        // flipped in between) still FAILS the mount over
                        // the handshake pipe — the daemon must never run
                        // with all logging discarded.
                        mount_bootstrap_fail(&format!(
                            "cannot open --log-file {}: {e}",
                            path.display()
                        ));
                    }
                }
            } else {
                None
            };
            if let Some(out_f) = output_file {
                use std::os::unix::io::AsRawFd;
                let fd = out_f.as_raw_fd();
                libc::dup2(fd, 1);
                libc::dup2(fd, 2);
            } else if let Ok(null_file) = std::fs::File::open("/dev/null") {
                use std::os::unix::io::AsRawFd;
                let fd = null_file.as_raw_fd();
                libc::dup2(fd, 1);
                libc::dup2(fd, 2);
            }
        }

        // Set daemon to false so the child process mounts in foreground
        if let Commands::Mount { ref mut daemon, .. } = &mut cli.command {
            *daemon = false;
        }
    }

    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            match &cli.command {
                Commands::Tune => {
                    eprintln!("Error: This command must be run as root (or with sudo).");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
    }

    #[cfg(unix)]
    if let Commands::Mount {
        args,
        mem_cache_size,
        disk_cache_size,
        disk_cache_paths: _,
        daemon: _,
        no_writeback,
        allow_other,
        options: _,
        read_cache_size: _,
        write_cache_size: _,
        read_mem_cache_size: _,
        write_mem_cache_size: _,
        mem_budget,
        fuse_io_uring_sqpoll_idle_ms: _,
        fuse_io_uring_sqpoll_cpu: _,
        meta_lv,
        ..
    } = &cli.command
    {
        // R5 (§5.7): the flag wins the budget resolution order. Parsed
        // before mount so the sampler's first tick already sees it.
        if let Some(ref mb) = mem_budget {
            let mut sys = sysinfo::System::new();
            sys.refresh_memory();
            match squeezefs::cache::parse_size_string(mb, sys.total_memory()) {
                Ok(bytes) => squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(bytes),
                Err(e) => mount_bootstrap_fail(&format!("invalid --mem-budget: {e}")),
            }
        }
        // NOTE: failures below may run in the daemonized CHILD (stderr →
        // /dev/null or the log file): `mount_bootstrap_fail` also reports
        // them over the handshake pipe so the parent prints the reason.
        let (meta_lvs, mountpoint) = if let Some(ref m_lvs) = meta_lv {
            if args.is_empty() {
                mount_bootstrap_fail("Mountpoint path is required");
            }
            let m_point = PathBuf::from(&args[0]);
            (m_lvs.clone(), m_point)
        } else {
            if args.len() < 2 {
                mount_bootstrap_fail(
                    "Metadata URI (sqmeta://...) and Mountpoint path are required",
                );
            }
            let mut m_lvs = Vec::new();
            for i in 0..(args.len() - 1) {
                match parse_block_uri(&args[i], "sqmeta://") {
                    Ok(parsed) => m_lvs.extend(parsed),
                    Err(e) => {
                        mount_bootstrap_fail(&format!("failed to parse Metadata URI: {}", e));
                    }
                }
            }
            let m_point = PathBuf::from(&args[args.len() - 1]);
            (m_lvs, m_point)
        };
        let fs_name = "squeezefs".to_string();
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            if !mountpoint.exists() {
                mount_bootstrap_fail(&format!("Mountpoint {:?} does not exist.", mountpoint));
            }
            use std::os::unix::fs::MetadataExt;
            match std::fs::metadata(&mountpoint) {
                Ok(meta) => {
                    if meta.uid() != uid {
                        mount_bootstrap_fail(&format!(
                            "Mountpoint {:?} is owned by UID {}, but current user is UID {}. \
                             Please use a mountpoint owned by you, or run with sudo.",
                            mountpoint,
                            meta.uid(),
                            uid
                        ));
                    }
                }
                Err(e) => {
                    mount_bootstrap_fail(&format!(
                        "Failed to read metadata of mountpoint {:?}: {}",
                        mountpoint, e
                    ));
                }
            }

            // Check if we are running in WSL and mounting under /mnt/ without allow_other
            let is_wsl = std::env::var("WSL_DISTRO_NAME").is_ok()
                || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                    .map(|s| s.to_lowercase().contains("microsoft"))
                    .unwrap_or(false);
            if is_wsl && mountpoint.starts_with("/mnt/") && !*allow_other {
                eprintln!("\x1b[93mWARNING\x1b[0m: Mounting a FUSE filesystem under '/mnt/' in WSL without '--allow-other'");
                eprintln!("         can cause the mount to hang or become unresponsive due to the WSL host file sharing service.");
                eprintln!("         Consider mounting under '/tmp/' or your home directory, or pass '--allow-other'");
                eprintln!(
                    "         (which requires uncommenting 'user_allow_other' in /etc/fuse.conf)."
                );
            }
        }

        let first_meta_path = &meta_lvs[0];
        let temp_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Version-gated bootstrap (PR K6a): the format config is read off
        // the SLOT-0 HOST's KV xattr tree via a read-only probe mount
        // (§5.5.1a discovery resolves the host — PR VL5b: after a slot-0
        // migration the config lives in a guest keyspace on another
        // member; legacy sets keep reading volume 0's ino 1 verbatim).
        // Blank, legacy-v2, foreign, torn, future-version, and
        // unknown-feature superblocks all fail loud here, before any
        // daemonization.
        let val_opt = temp_rt.block_on(async {
            let disc = squeezefs::meta_backend::discover_meta_set(&meta_lvs).await?;
            let home = &disc.ordered_paths[disc.slot_to_volume[0]];
            let vol = squeezefs::meta_backend::open_volume_probe(home).await?;
            let root = vol.slot0_root_ino();
            vol.getxattr(
                root,
                squeezefs::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
            )
            .await
        });
        let val_opt = match val_opt {
            Ok(v) => v,
            Err(e) => {
                mount_bootstrap_fail(&format!(
                    "Failed to read format config from metadata volume {}: {}",
                    first_meta_path, e
                ));
            }
        };

        let format_config: FormatConfig = match val_opt {
            Some(val) => match serde_json::from_slice(&val) {
                Ok(cfg) => cfg,
                Err(e) => {
                    mount_bootstrap_fail(&format!("Failed to parse format config: {}", e));
                }
            },
            None => {
                mount_bootstrap_fail(
                    "Format configuration not found on root inode. Is this volume formatted?",
                );
            }
        };

        // VAL-3 (docs/design-key-handling.md §4/§6): resolve the
        // encryption key BEFORE any runtime or FUSE machinery spins up —
        // a pre-KW-1 (RSA-wrap) volume refuses loud with its remedy, and a
        // missing/wrong key file names every source instead of failing
        // every read later. The resolved key is re-derived at FUSE init
        // from the same sources (the material crossed the fork in
        // memory), so this is a preflight, not the only gate.
        if let Err(msg) = squeezefs::keyfile::mount_volume_key(&format_config) {
            mount_bootstrap_fail(&msg);
        }

        let resolved_mem_cache_size = mem_cache_size
            .as_deref()
            .unwrap_or_else(|| format_config.mem_cache_size.as_deref().unwrap_or("1GB"));
        let resolved_disk_cache_size = disk_cache_size
            .as_deref()
            .unwrap_or_else(|| format_config.disk_cache_size.as_deref().unwrap_or("10GB"));

        // Cache paths come from the format config ONLY (mount overrides
        // were rejected above). No paths declared at format ⇒ the
        // filesystem is permanently cache-less: no default dir is
        // conjured, no staging/read-cache tier exists.
        let staging_dirs = format_config.disk_cache_paths.clone().unwrap_or_default();

        // Staging WRITABILITY preflight: the daemon identity (the user
        // running this process — --uid/--gid are presentation-only) must
        // be able to use every declared root. Fails LOUD with the chown
        // remedy here — before the summary, the runtime, and any FUSE
        // setup — never as a raw EACCES mid-bootstrap. On a daemonized
        // child the message reaches the parent via the handshake pipe.
        if let Err(msg) = squeezefs::config_ops::staging_write_preflight(&staging_dirs) {
            mount_bootstrap_fail(&msg);
        }

        // FIND-RW4-A geometry pre-flight (the authoritative gate re-runs at
        // FUSE init): refuse pre-fix transformed geometry before any
        // runtime/FUSE machinery spins up. Key material is irrelevant to
        // the arithmetic (the check uses the conservative envelope bound).
        {
            let probe = squeezefs::crypto_compress::CryptoCompressState::new(
                format_config.compression.clone(),
                format_config.encrypt_algo.clone(),
                None,
            );
            if let Err(msg) = probe.transform_geometry_check(
                format_config.block_size,
                squeezefs::block_allocator::CHUNK_SIZE,
            ) {
                mount_bootstrap_fail(&msg);
            }
        }

        let data_lvs = format_config.data_lv.clone().unwrap_or_default();
        let writeback = !no_writeback;

        print_squeezefs_summary(
            false,
            &fs_name,
            format_config.block_size,
            format_config.capacity,
            &format_config.compression,
            &format_config.encrypt_algo,
            &meta_lvs,
            &data_lvs,
            resolved_mem_cache_size,
            resolved_disk_cache_size,
            &staging_dirs,
            Some(&mountpoint),
            Some(writeback),
            Some(*allow_other),
        );
    }

    // ENG-3 (the daemon must be audible): default filter is `info` when
    // RUST_LOG is unset — a stock mount at Error-only silently discarded
    // reservation preemptions, the job-wire security notice, shard lease
    // expiries, the O_DIRECT→buffered degradation, checkpoint/bitmap
    // write failures and teardown failures. An explicit RUST_LOG wins
    // verbatim (the env-knob convention).
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Some(ref log_path) = cli.log_file {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(e) => {
                // Preflighted at startup; a failure here is still never
                // swallowed — logging must go where the operator asked.
                eprintln!("Error: cannot open --log-file {}: {e}", log_path.display());
                std::process::exit(1);
            }
        }
    }
    builder.init();

    // ENG-8: a profiling build must announce itself the moment logging is
    // live. `--all-features` compiles `dhat-on`, which replaces jemalloc
    // (and drops the load-bearing `dirty_decay_ms:1000`) — anyone who
    // mounts or benchmarks that binary is measuring a different allocator.
    // The `--version` line carries the same notice for evidence notes that
    // capture it.
    if let Some(warning) =
        squeezefs::version::profiling_build_warning(squeezefs::version::measurement_disqualifiers())
    {
        log::warn!("{warning}");
    }

    // NOW start the Tokio runtime in the surviving process
    let mut core_ids = core_affinity::get_core_ids().unwrap_or_default();
    if !core_ids.is_empty() {
        core_ids.remove(0); // Reserve Core 0 for OS kernel tasks
    }
    let core_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
    if !core_ids.is_empty() {
        rt_builder.worker_threads(core_ids.len());
    }
    let rt = rt_builder
        .enable_all()
        .max_blocking_threads(8192)
        .on_thread_start(move || {
            let idx = core_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if idx < core_ids.len() {
                core_affinity::set_for_current(core_ids[idx]);
            }
        })
        .build()
        .unwrap();

    let res = rt.block_on(async { run_app(cli).await });
    if let Err(ref e) = res {
        #[cfg(unix)]
        {
            let fd = DAEMON_PIPE.load(std::sync::atomic::Ordering::Relaxed);
            if fd >= 0 {
                let msg = format!("Error: {}\n", e);
                let _ = unsafe { libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len()) };
                let _ = unsafe { libc::close(fd) };
            }
        }
    }
    res
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    const TB: u64 = 1024 * 1024 * 1024 * 1024;
    const PB: u64 = 1024 * 1024 * 1024 * 1024 * 1024;

    if bytes >= PB {
        format!("{:.2} PB", bytes as f64 / PB as f64)
    } else if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn halve_size_string(val: &str, default_fallback: &str) -> String {
    let s = val.trim();
    if let Some(stripped) = s.strip_suffix('%') {
        if let Ok(p) = stripped.trim().parse::<f64>() {
            return format!("{:.1}%", p / 2.0);
        }
    }
    if let Ok(bytes) = squeezefs::cache::parse_size_string(s, 0) {
        format_size(bytes / 2)
    } else {
        default_fallback.to_string()
    }
}

fn print_squeezefs_summary(
    is_format: bool,
    fs_name: &str,
    block_size: u64,
    capacity: u64,
    compression: &str,
    encrypt_algo: &str,
    meta_lvs: &[String],
    data_lvs: &[String],
    mem_cache_size: &str,
    disk_cache_size: &str,
    disk_cache_paths: &[PathBuf],
    mountpoint: Option<&Path>,
    writeback: Option<bool>,
    allow_other: Option<bool>,
) {
    let mode_str = if is_format {
        "Format Configuration Summary".bold().yellow()
    } else {
        "Mount Configuration Summary".bold().green()
    };
    println!(
        "SqueezeFS version {}",
        squeezefs::version::version_line().bold().cyan()
    );
    println!(
        "{}",
        "======================================================================".blue()
    );
    println!("  {}", mode_str);
    println!(
        "{}",
        "----------------------------------------------------------------------".blue()
    );
    println!("  {:<20} {}", "Volume Name:", fs_name.bold());
    println!("  {:<20} {}", "Capacity:", format_size_human(capacity));
    println!("  {:<20} {}", "Block Size:", format_size_human(block_size));
    println!("  {:<20} {}", "Compression:", compression);
    println!("  {:<20} {}", "Encryption:", encrypt_algo);
    println!(
        "{}",
        "----------------------------------------------------------------------".blue()
    );
    println!("{}", "Metadata Volumes:".bold().cyan());
    for path in meta_lvs {
        println!(
            "  - [{}] {} (health: {})",
            "enabled".green(),
            path,
            "1000".yellow()
        );
    }
    println!("{}", "Data Volumes:".bold().cyan());
    for path in data_lvs {
        println!(
            "  - [{}] {} (health: {})",
            "enabled".green(),
            path,
            "1000".yellow()
        );
    }
    println!(
        "{}",
        "----------------------------------------------------------------------".blue()
    );
    println!("{}", "Cache/Staging:".bold().cyan());
    println!("  {:<20} {}", "Memory Cache Size:", mem_cache_size);
    println!("  {:<20} {}", "Disk Cache Size:", disk_cache_size);
    let paths_str = disk_cache_paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "  {:<20} {}",
        "Staging Paths:",
        if paths_str.is_empty() {
            "none (cache-less)".yellow()
        } else {
            paths_str.normal()
        }
    );

    if !is_format {
        if let Some(mp) = mountpoint {
            println!(
                "{}",
                "----------------------------------------------------------------------".blue()
            );
            println!("{}", "Mount Options:".bold().cyan());
            println!("  {:<20} {:?}", "Mountpoint:", mp);
            if let Some(wb) = writeback {
                println!(
                    "  {:<20} {}",
                    "Writeback Cache:",
                    if wb {
                        "enabled".green()
                    } else {
                        "disabled".yellow()
                    }
                );
            }
            if let Some(ao) = allow_other {
                println!(
                    "  {:<20} {}",
                    "Allow Other:",
                    if ao {
                        "enabled".green()
                    } else {
                        "disabled".yellow()
                    }
                );
            }
        }
    }
    println!(
        "{}",
        "======================================================================".blue()
    );
}

/// Resolve the filesystem's formatted capacity from the summed physical
/// data-volume sizes and the optional `--capacity` request.
///
/// **Oversubscription is refused at this layer**: SqueezeFS will not
/// advertise more bytes than its data volumes physically provide (a 400 GiB
/// pool can no longer be formatted as 1 PiB — the legacy behavior even
/// defaulted to a fabricated 1 PiB when probing failed). Thin provisioning
/// belongs underneath (LVM thin pools, fabric namespaces), where the
/// operator owns the overcommit. Undersubscription (`--capacity` below the
/// physical total, e.g. for testing) and inode-count control are unaffected.
fn resolve_format_capacity(physical_total: u64, requested: Option<u64>) -> Result<u64, String> {
    if physical_total == 0 {
        return Err(
            "could not determine the physical size of any data volume; refusing to format \
             (capacity oversubscription is not supported — check the sqdata:// paths/sizes)"
                .to_string(),
        );
    }
    match requested {
        None => Ok(physical_total),
        Some(0) => Err("requested --capacity of 0 bytes".to_string()),
        Some(req) if req > physical_total => Err(format!(
            "requested --capacity {} exceeds the physical data-volume total {} — \
             oversubscription is not supported at the filesystem level; lower --capacity, \
             add data volumes, or thin-provision underneath (LVM/fabric)",
            format_size_human(req),
            format_size_human(physical_total)
        )),
        Some(req) => Ok(req),
    }
}

fn get_backing_device_size(path: &str) -> std::io::Result<u64> {
    use std::fs::File;
    use std::io::Seek;

    let mut file = File::open(path)?;
    // Try seeking to the end
    if let Ok(size) = file.seek(std::io::SeekFrom::End(0)) {
        if size > 0 {
            return Ok(size);
        }
    }

    // Fallback to metadata length if seek returns 0 or fails
    let meta = std::fs::metadata(path)?;
    Ok(meta.len())
}

fn format_size_human(bytes: u64) -> String {
    let kib = bytes as f64 / 1024.0;
    let mib = kib / 1024.0;
    let gib = mib / 1024.0;
    let tib = gib / 1024.0;
    let pib = tib / 1024.0;

    if pib >= 1.0 {
        format!("{:.2} PiB", pib)
    } else if tib >= 1.0 {
        format!("{:.2} TiB", tib)
    } else if gib >= 1.0 {
        format!("{:.2} GiB", gib)
    } else if mib >= 1.0 {
        format!("{:.2} MiB", mib)
    } else if kib >= 1.0 {
        format!("{:.2} KiB", kib)
    } else {
        format!("{} B", bytes)
    }
}

fn parse_human_readable_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Empty size string".to_string());
    }

    let mut num_str = s;
    let mut multiplier = 1u64;

    if let Some(last_char) = s.chars().last() {
        if !last_char.is_ascii_digit() {
            num_str = &s[..s.len() - 1];
            multiplier = match last_char.to_ascii_lowercase() {
                'k' => 1024,
                'm' => 1024 * 1024,
                'g' => 1024 * 1024 * 1024,
                't' => 1024 * 1024 * 1024 * 1024,
                'p' => 1024 * 1024 * 1024 * 1024 * 1024,
                _ => return Err(format!("Invalid size suffix '{}'", last_char)),
            };
        }
    }

    let base_val: u64 = num_str
        .trim()
        .parse()
        .map_err(|e| format!("Invalid number '{}': {}", num_str, e))?;
    Ok(base_val * multiplier)
}

/// The top-level `squeezefs nvmeof` dispatch (design §6.2 grammar). Kept
/// out of the giant command match so the NvmeofError Display text — the
/// designed loud-fail/runbook UX — can be rendered verbatim by the caller.
fn dispatch_nvmeof(action: NvmeofActions) -> Result<(), squeezefs::nvmeof::stack::NvmeofError> {
    use squeezefs::nvmeof::stack::NvmeofError;
    match action {
        NvmeofActions::Share {
            backing_path,
            subnqn,
            port,
            ip,
            target_stack,
            nsid,
            ns_uuid,
            create_size,
            allow_host,
            accept_version_drift,
        } => {
            let create_size = match create_size {
                Some(raw) => {
                    // Accept both "10G" and "10GB" spellings.
                    let normalized = raw.trim().trim_end_matches(['b', 'B']).to_string();
                    Some(parse_human_readable_size(&normalized).map_err(|e| {
                        NvmeofError::Refused(format!("--create-size '{raw}' is invalid: {e}"))
                    })?)
                }
                None => None,
            };
            let record = squeezefs::nvmeof::share(&squeezefs::nvmeof::ShareOptions {
                backing_path: backing_path.clone(),
                subnqn,
                port,
                ips: ip.clone(),
                stack: target_stack.map(Into::into),
                nsid,
                ns_uuid,
                create_size,
                allow_hosts: allow_host,
                accept_version_drift,
            })?;
            println!(
                "Successfully shared '{}' as an NVMe-oF target ({} stack).",
                backing_path,
                record.stack.as_str()
            );
            println!("Subsystem NQN: {}", record.subnqn);
            if let Some(u) = &record.ns_uuid {
                println!("Namespace UUID: {u}");
            }
            println!("Connection string for client nodes:");
            println!(
                "  squeezefs nvmeof connect --ip {} --port {} --subnqn {}",
                ip.first()
                    .cloned()
                    .unwrap_or_else(|| "<your-target-ip>".to_string()),
                port,
                record.subnqn
            );
        }
        NvmeofActions::Unshare {
            subnqn,
            force,
            accept_version_drift,
        } => {
            squeezefs::nvmeof::unshare(&subnqn, force, accept_version_drift)?;
            println!("Successfully stopped sharing target NQN '{}'.", subnqn);
        }
        NvmeofActions::List { json } => {
            squeezefs::nvmeof::list(json)?;
        }
        NvmeofActions::Restore {
            target_stack,
            accept_version_drift,
        } => {
            squeezefs::nvmeof::restore(target_stack.map(Into::into), accept_version_drift)?;
        }
        NvmeofActions::Adopt {
            subnqn,
            target_stack,
        } => {
            let record = squeezefs::nvmeof::adopt(&subnqn, target_stack.map(Into::into))?;
            println!(
                "Adopted '{}' into management on the {} stack (provenance class: {}) — target \
                 state untouched, only the share ledger was written.",
                record.subnqn,
                record.stack.as_str(),
                record
                    .adopted_from
                    .as_ref()
                    .map(|a| a.class.as_str())
                    .unwrap_or("-"),
            );
            println!(
                "The share is now fully managed: 'nvmeof list' shows it with its provenance, \
                 and 'nvmeof restore' / 'nvmeof unshare {}' apply.",
                record.subnqn
            );
        }
        NvmeofActions::Connect {
            ip,
            port,
            subnqn,
            host_traddr,
            host_iface,
            nr_io_queues,
        } => {
            println!("Connecting to NVMe-oF target at {}:{}...", ip, port);
            let opts = squeezefs::nvmeof::ConnectOptions {
                host_traddr,
                host_iface,
                nr_io_queues,
            };
            let dev = squeezefs::nvmeof::connect_target(&ip, port, &subnqn, &opts)?;
            if dev.starts_with("/dev/") {
                println!("{}", "Connection successful!".green().bold());
                println!("Attached Remote Disk: {}", dev.cyan().bold());
            } else {
                println!("{}", dev.yellow());
            }
        }
        NvmeofActions::Disconnect { subnqn } => {
            squeezefs::nvmeof::disconnect_target(&subnqn)?;
            println!("Successfully disconnected from target NQN '{}'.", subnqn);
        }
        NvmeofActions::Target { action } => match action {
            TargetActions::Install {
                version,
                with_pkgdep,
            } => {
                squeezefs::nvmeof::target_install(version.as_deref(), with_pkgdep)?;
            }
            TargetActions::Setup {
                hugemem_mb,
                restore_prior,
                target_stack,
            } => {
                squeezefs::nvmeof::target_setup(
                    target_stack.map(Into::into),
                    hugemem_mb,
                    restore_prior,
                )?;
            }
            TargetActions::Start {
                target_stack,
                core_mask,
                cores,
                dpdk_mem_mb,
                accept_version_drift,
            } => {
                squeezefs::nvmeof::target_start(
                    target_stack.map(Into::into),
                    &squeezefs::nvmeof::TargetStartOptions {
                        core_mask,
                        cores,
                        dpdk_mem_mb,
                        accept_version_drift,
                    },
                )?;
            }
            TargetActions::Stop {
                force,
                target_stack,
            } => {
                squeezefs::nvmeof::target_stop(target_stack.map(Into::into), force)?;
            }
            TargetActions::Status { json, target_stack } => {
                squeezefs::nvmeof::target_status(target_stack.map(Into::into), json)?;
            }
            TargetActions::SystemdUnit {
                target_stack,
                core_mask,
                cores,
                dpdk_mem_mb,
            } => {
                squeezefs::nvmeof::target_systemd_unit(
                    target_stack.map(Into::into),
                    core_mask,
                    cores,
                    dpdk_mem_mb,
                )?;
            }
        },
    }
    Ok(())
}

async fn run_app(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Commands::Format {
            uris,
            block_size,
            capacity,
            mem_cache_size,
            disk_cache_size,
            disk_cache_paths,
            data_lv,
            meta_lv,
            ip: _,
            port: _,
            subnqn: _,
            force,
            full,
            inodes,
            compression,
            encrypt_algo,
            encrypt_key,
            read_cache_size,
            write_cache_size,
            read_mem_cache_size,
            write_mem_cache_size,
            dismount_wait,
            upload_delay,
            fuse_io_uring_sqpoll_idle_ms,
            meta_node_kib,
            meta_journal_mb,
            meta_slots,
        } => {
            // Dynamic meta routing (design-dynamic-meta-routing §5.1,
            // forward-only): the width is DERIVED — the retired
            // `--meta-slots` knob refuses loud naming its successor
            // BEFORE any destructive step.
            if let Some(w) = meta_slots {
                return Err(format!(
                    "--meta-slots {w}: RETIRED — metadata routing widths are derived, never \
                     chosen (dynamic meta routing): every format freezes the derived \
                     virtual width ({} slots) and spreads minting so growth needs no \
                     planning; grow with `squeezefs volume add-meta --take-slots ...` \
                     (offline) or `squeezefs volume migrate-meta-slot` (online)",
                    squeezefs::meta_backend::DERIVED_ROUTING_WIDTH
                )
                .into());
            }

            // VAL-3 + KW-1 (docs/design-key-handling.md): the key comes
            // from a FILE (or stdin), never from argv, and only its KDF
            // salt + key id are ever persisted. Resolved BEFORE any
            // destructive step so a bad key file costs nothing.
            let encrypt_mode = squeezefs::crypto_compress::EncryptMode::parse(&encrypt_algo)
                .map_err(|e| e.to_string())?;
            let canonical_encrypt_algo = encrypt_mode.as_str().to_string();
            let encrypt_key_ref = if encrypt_mode != squeezefs::crypto_compress::EncryptMode::None {
                let spec = encrypt_key.as_deref().ok_or_else(|| {
                    format!(
                        "--encrypt-algo {canonical_encrypt_algo} requires --encrypt-key \
                         <path> (the path to a key FILE, or \"-\" to read the key from \
                         stdin). Generate one with `head -c 32 /dev/urandom | base64 > \
                         volume.key && chmod 600 volume.key`. The key never lands on the \
                         volume and never rides argv — keep the file: mount needs it."
                    )
                })?;
                let material = squeezefs::keyfile::read_key_source(spec)?;
                let salt = squeezefs::keyfile::new_kdf_salt();
                let volume_key = squeezefs::keyfile::derive_volume_key(&material, &salt);
                let key_id = volume_key.key_id_hex();
                println!(
                    "Encryption: {canonical_encrypt_algo}, key id {key_id} (wrap scheme \
                     {}).\n  The key file is NOT stored on the volume — only a KDF salt \
                     and this id are.\n  To mount: `--encrypt-key <path>`, \
                     `{}=<path>`, or place the same key file at {}.",
                    squeezefs::keyfile::KEY_WRAP_SCHEME_V2,
                    squeezefs::keyfile::KEY_FILE_ENV,
                    squeezefs::keyfile::default_key_path(&key_id).display(),
                );
                Some(squeezefs::keyfile::make_key_ref(&salt, &volume_key))
            } else {
                if encrypt_key.is_some() {
                    return Err("--encrypt-key was given but --encrypt-algo is \"none\": \
                                nothing would be encrypted. Pass --encrypt-algo \
                                aes256gcm (or chacha20), or drop --encrypt-key."
                        .into());
                }
                None
            };

            let mut meta_lvs = Vec::new();
            let mut data_lvs = Vec::new();

            for uri in &uris {
                if uri.starts_with("sqmeta://") {
                    meta_lvs.extend(parse_block_uri(uri, "sqmeta://")?);
                } else if uri.starts_with("sqdata://") {
                    data_lvs.extend(parse_block_uri(uri, "sqdata://")?);
                } else {
                    return Err(format!(
                        "Error: Invalid URI scheme in '{}'. Must start with 'sqmeta://' or 'sqdata://'",
                        uri
                    )
                    .into());
                }
            }

            if let Some(ref m_lvs) = meta_lv {
                meta_lvs.extend(m_lvs.clone());
            }
            if let Some(ref d_lvs) = data_lv {
                data_lvs.extend(d_lvs.clone());
            }

            if meta_lvs.is_empty() {
                return Err(
                    "Error: At least one Metadata URI (sqmeta://...) or --meta-lv option is required"
                        .into(),
                );
            }
            if data_lvs.is_empty() {
                return Err(
                    "Error: At least one Data URI (sqdata://...) or --data-lv option is required"
                        .into(),
                );
            }
            let _ctrl_c_guard = spawn_ctrl_c_handler("formatting");

            // Type-check every data volume up front so a bogus path (char
            // device, directory, empty file, missing path) fails with a
            // precise error instead of a downstream capacity complaint.
            for path in &data_lvs {
                squeezefs::storage::validate_backing_device(path)
                    .map_err(|e| format!("data volume '{}' failed validation: {}", path, e))?;
            }

            let mut physical_total = 0;
            for path in &data_lvs {
                if let Ok(size) = get_backing_device_size(path) {
                    physical_total += size;
                }
            }
            println!(
                "Total physical data volume capacity: {}",
                format_size_human(physical_total)
            );
            let requested = match capacity {
                Some(ref cap_str) => Some(parse_human_readable_size(cap_str)?),
                None => None,
            };
            // No oversubscription at this layer: advertised capacity is
            // bounded by physical backing (undersubscribe freely for tests).
            let total_capacity = resolve_format_capacity(physical_total, requested)?;
            if total_capacity != physical_total {
                println!(
                    "Formatted capacity (requested): {}",
                    format_size_human(total_capacity)
                );
            }

            // v3 metadata-format knobs (design-cow-kv-metadata §5.1): the
            // node-size allowed set with the sub-256 KiB record-cap
            // warning, and the --meta-journal-mb override of the resolved
            // OQ 1 ring clamp.
            let (meta_node_size, node_kib_warning) =
                squeezefs::meta_backend::kv::superblock::validate_node_kib(meta_node_kib)
                    .map_err(squeezefs::error::SqueezefsError::from)?;
            if let Some(warning) = node_kib_warning {
                eprintln!("\x1b[93mWARNING\x1b[0m: {warning}");
            }
            let meta_journal_override = meta_journal_mb.map(|mb| mb * 1024 * 1024);

            let meta_slot_plan = squeezefs::meta_backend::plan_meta_slot_set(meta_lvs.len())
                .map_err(|e| format!("meta slot plan: {e}"))?;
            println!(
                "Meta routing: derived virtual width W = {} over {} volume(s), mint spread \
                 {} slots/volume — grow any time with `volume add-meta` / \
                 `migrate-meta-slot` (no planning knob; pre-dynamic-routing binaries \
                 refuse this set).",
                meta_slot_plan.routing_width,
                meta_lvs.len(),
                squeezefs::meta_backend::MINT_SPREAD
                    .min((meta_slot_plan.routing_width as usize).div_ceil(meta_lvs.len().max(1))),
            );

            let requested_block_size = parse_human_readable_size(&block_size)?;
            // Every logical block lives in one fixed allocator chunk: a
            // larger block would span chunks and corrupt its neighbor.
            let chunk = squeezefs::block_allocator::CHUNK_SIZE;
            if requested_block_size > chunk {
                return Err(format!(
                    "--block-size {} exceeds the {} B allocator chunk — blocks must fit \
                     one chunk",
                    block_size, chunk
                )
                .into());
            }
            // FIND-RW4-A: transformed (compressed/encrypted) volumes must
            // reserve per-chunk headroom so a full block stored RAW (the
            // incompressible-block escape) plus its frame/AEAD envelope
            // still fits the chunk. Clamp loudly; mounts REFUSE transformed
            // volumes that violate this geometry.
            let transform_active = squeezefs::crypto_compress::CompressionMode::parse(&compression)
                .map_err(|e| e.to_string())?
                != squeezefs::crypto_compress::CompressionMode::None
                || encrypt_mode != squeezefs::crypto_compress::EncryptMode::None;
            let transform_cap = chunk - squeezefs::crypto_compress::TRANSFORM_BLOCK_HEADROOM;
            let parsed_block_size = if transform_active && requested_block_size > transform_cap {
                println!(
                    "Block size clamped to {} B (requested {}): compressed/encrypted \
                     volumes reserve {} B of per-chunk headroom so incompressible \
                     blocks always fit their allocator chunk (FIND-RW4-A).",
                    transform_cap,
                    block_size,
                    squeezefs::crypto_compress::TRANSFORM_BLOCK_HEADROOM
                );
                transform_cap
            } else {
                requested_block_size
            };
            let config = FormatConfig {
                name: "squeezefs".to_string(),
                block_size: parsed_block_size,
                capacity: total_capacity,
                inodes,
                compression: compression.clone(),
                // KW-1: the canonical spelling, never a `-rsa` name.
                encrypt_algo: canonical_encrypt_algo.clone(),
                // VAL-3: key MATERIAL never reaches the volume — this
                // legacy field is write-never (skip_serializing) and only
                // the salt + id below are persisted.
                encrypt_key: None,
                encrypt_key_ref: encrypt_key_ref.clone(),
                mem_cache_size: mem_cache_size.clone(),
                disk_cache_size: disk_cache_size.clone(),
                disk_cache_paths: disk_cache_paths.clone(),
                data_lv: Some(data_lvs.clone()),
                // Fresh formats stay legacy-shaped (KD-14 zero-cost
                // grandfathering): durable records materialize — and the
                // lifecycle bit stamps — only at the first lifecycle verb.
                data_volumes: None,
                read_cache_size: read_cache_size.clone(),
                write_cache_size: write_cache_size.clone(),
                read_mem_cache_size: read_mem_cache_size.clone(),
                write_mem_cache_size: write_mem_cache_size.clone(),
                dismount_wait: dismount_wait.clone(),
                upload_delay: Some(upload_delay.clone()),
                fuse_io_uring_sqpoll_idle_ms,
                // PR VL5a mirrors (the §5.5.1a stamps are authoritative;
                // None on default formats keeps the config byte-identical).
                meta_routing_width: Some(meta_slot_plan.routing_width),
                meta_slot_runs: Some(
                    meta_slot_plan
                        .stamps
                        .iter()
                        .map(|st| {
                            st.slots_hosted
                                .runs()
                                .iter()
                                .map(|r| (r.start, r.stride, r.count))
                                .collect()
                        })
                        .collect(),
                ),
                meta_volumes: Some({
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    meta_lvs
                        .iter()
                        .enumerate()
                        .map(|(i, path)| squeezefs::MetaVolumeRecord {
                            id: squeezefs::new_data_volume_id(),
                            backing_dev: path.clone(),
                            member_position: i as u16,
                            added_ts: now,
                        })
                        .collect()
                }),
            };

            // Pre-flight EVERY meta volume before ANY destructive step
            // (staging-dir wipes, data zeroing, meta wipes): a refused format
            // must leave all volumes and caches intact.
            for path in &meta_lvs {
                squeezefs::meta_backend::kv::builder::format_preflight(Path::new(path), force)
                    .await
                    .map_err(|e| format!("metadata volume '{}': {}", path, e))?;
            }

            // Wipe + recreate + OWNERSHIP-STAMP every declared staging
            // root (config_ops::stamp_staging_dirs): owned by the INVOKING
            // user under sudo (SUDO_UID:SUDO_GID), root only for a genuine
            // root deployment — a later user-mode mount must never EACCES
            // on its own staging. ENG-4 wipe guard: system paths are
            // hard-refused and non-empty unmarked dirs need consent —
            // format's own `--force` doubles as the wipe consent; every
            // dir prechecks before any is wiped.
            if let Some(ref paths) = disk_cache_paths {
                squeezefs::config_ops::stamp_staging_dirs(paths, force).await?;
            }

            let quick = !full;
            let num_cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let pool_size = num_cores * 2;
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(pool_size));
            let mp = std::sync::Arc::new(indicatif::MultiProgress::new());

            let mut join_handles = Vec::new();

            // 1. Concurrent Metadata Volumes Tasks — format v3 (design
            // §5.1: the CLI produces only v3 from PR K6a on; v2 formatting
            // is test-surface-only, §6.2/resolved OQ 4). The volume-set
            // format config is recorded in the FIRST volume's v3 xattr
            // tree (the mount bootstrap reads it back through the
            // dual-format dispatch).
            let config_bytes = serde_json::to_vec(&config)?;
            let first_meta = meta_lvs[0].clone();
            for (position, path) in meta_lvs.clone().into_iter().enumerate() {
                let sem = semaphore.clone();
                let config_xattr = if path == first_meta {
                    Some(config_bytes.clone())
                } else {
                    None
                };
                // PR VL5a: each member's §5.5.1a stamp, by format order
                // (= member_position; the canonical set order).
                let stamp = meta_slot_plan.stamps[position].clone();
                let handle = tokio::task::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    // Volume length: block devices use their physical
                    // size; regular files grow to the v2-era 128 MiB
                    // floor (format_v3 set_lens them).
                    let physical = get_backing_device_size(&path).unwrap_or(0);
                    let volume_len = std::cmp::max(physical, 128 * 1024 * 1024);
                    if !quick {
                        println!("Full-wiping metadata volume {path} ({volume_len} bytes)...");
                    }
                    let opts = squeezefs::meta_backend::kv::builder::FormatV3Options {
                        node_size: meta_node_size,
                        journal_len_override: meta_journal_override,
                        force,
                        full_wipe: !quick,
                        format_config_xattr: config_xattr,
                    };
                    squeezefs::meta_backend::kv::builder::format_v3_stamped(
                        Path::new(&path),
                        volume_len,
                        &opts,
                        stamp,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("Failed to format metadata volume '{}': {}", path, e))?;
                    Ok::<(), String>(())
                });
                join_handles.push(handle);
            }

            // 2. Concurrent Data Volumes Tasks
            for path in data_lvs.clone() {
                let sem = semaphore.clone();
                let mp_c = mp.clone();
                let path_basename = Path::new(&path)
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("data"))
                    .to_string_lossy()
                    .to_string();

                let handle = tokio::task::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    tokio::task::spawn_blocking(move || {
                        let physical_size = get_backing_device_size(&path).unwrap_or(0);
                        let wipe_len = if quick {
                            std::cmp::min(total_capacity, 32 * 1024 * 1024)
                        } else if physical_size > 0 {
                            std::cmp::min(total_capacity, physical_size)
                        } else {
                            total_capacity
                        };

                        let pb = if !quick {
                            let pb = mp_c.add(indicatif::ProgressBar::new(wipe_len));
                            pb.set_style(
                                indicatif::ProgressStyle::default_bar()
                                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta}) - {msg}")
                                    .unwrap()
                                    .progress_chars("#>-")
                            );
                            pb.set_message(format!("Data: {}", path_basename));
                            Some(pb)
                        } else {
                            None
                        };

                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .map_err(|e| format!("Failed to open data volume '{}': {}", path, e))?;
                        use std::io::Write;
                        let zeros = vec![0u8; 1024 * 1024];
                        let mut written = 0;
                        while written < wipe_len {
                            let to_write =
                                std::cmp::min(zeros.len() as u64, wipe_len - written) as usize;
                            file.write_all(&zeros[..to_write])
                                .map_err(|e| format!("Failed to write to data volume '{}': {}", path, e))?;
                            written += to_write as u64;
                            if let Some(ref p_bar) = pb {
                                p_bar.inc(to_write as u64);
                            }
                        }
                        file.sync_all().map_err(|e| format!("Failed to sync data volume '{}': {}", path, e))?;
                        if let Some(ref p_bar) = pb {
                            p_bar.finish_with_message("Complete");
                        }
                        Ok::<(), String>(())
                    })
                    .await
                    .map_err(|e| e.to_string())?
                });
                join_handles.push(handle);
            }

            for handle in join_handles {
                handle.await.map_err(|e| e.to_string())??;
            }

            log::info!("Successfully formatted (v3) and recorded config on metadata volume.");
            let resolved_mem = mem_cache_size.as_deref().unwrap_or("1GB");
            let resolved_disk = disk_cache_size.as_deref().unwrap_or("10GB");
            // No `--disk-cache-paths` ⇒ the filesystem is permanently
            // cache-less (the summary prints "none (cache-less)"); mounts
            // never conjure a default staging dir.
            let resolved_paths = disk_cache_paths.clone().unwrap_or_default();

            print_squeezefs_summary(
                true,
                "squeezefs",
                parsed_block_size,
                total_capacity,
                &compression,
                &canonical_encrypt_algo,
                &meta_lvs,
                &data_lvs,
                resolved_mem,
                resolved_disk,
                &resolved_paths,
                None,
                None,
                None,
            );

            println!("Format complete.");
        }
        Commands::Status { meta_uri } => {
            let path = if let Some(ref uri) = meta_uri {
                if uri.starts_with("sqmeta://") {
                    parse_block_uri(uri, "sqmeta://")?[0].clone()
                } else {
                    uri.clone()
                }
            } else {
                let mounts = find_squeezefs_mounts();
                if mounts.is_empty() {
                    return Err(
                        "Error: no metadata volume path specified or active mount found".into(),
                    );
                }
                let config_path = mounts[0].join(".config");
                let config_str = std::fs::read_to_string(&config_path)?;
                let config_json: serde_json::Value = serde_json::from_str(&config_str)?;
                // Check format data cache paths or active devices
                let meta_path = config_json["format"]["disk_cache_paths"]
                    .as_str()
                    .unwrap_or("");
                // Since meta_lv is not stored directly in FormatConfig, let's look for isolated segment dir or default meta path if we can find it
                // Actually, if we just use the first metadata volume path from standard proc mounts or mounts, or return an error if no URI is provided.
                if meta_path.is_empty() {
                    return Err("Error: no metadata volume path specified".into());
                }
                meta_path.split(',').collect::<Vec<_>>()[0].to_string()
            };
            squeezefs::set_fs_prefix("squeezefs");
            let status = squeezefs::fuse_client::get_volume_status(&path).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Commands::Clients { meta_uri, json } => {
            squeezefs::set_fs_prefix("squeezefs");
            let meta_lvs = if meta_uri.starts_with("sqmeta://") {
                parse_block_uri(&meta_uri, "sqmeta://")?
            } else {
                vec![meta_uri]
            };
            run_clients_report(&meta_lvs, json).await?;
        }
        Commands::Volume { action } => {
            squeezefs::set_fs_prefix("squeezefs");
            let live = |t: &str| !t.starts_with("sqmeta://");
            match action {
                VolumeActions::AddData {
                    target,
                    device,
                    no_rebalance,
                } => {
                    // §5.3 step 6 (KD-12): the auto-rebalance pass is the
                    // DEFAULT — armed by this PR (VL4).
                    let rebalance_note = if no_rebalance {
                        "rebalance: opted out (--no-rebalance)".to_string()
                    } else if live(&target) {
                        "rebalance: automatic bounded pass submitted at 25 % throttle \
                         (design-volume-lifecycle §5.3 step 6; see `squeezefs job list`)"
                            .to_string()
                    } else {
                        "rebalance: durable queued rebalance job recorded — the pass runs \
                         when the set is next mounted (design-volume-lifecycle §5.3 step 6)"
                            .to_string()
                    };
                    if live(&target) {
                        let arg = if no_rebalance {
                            format!("{device} no-rebalance")
                        } else {
                            device.clone()
                        };
                        let body = admin_roundtrip(&target, "volume-add-data", &arg)?;
                        let v: serde_json::Value = serde_json::from_str(&body)
                            .map_err(|e| format!("undecodable admin reply: {e}"))?;
                        println!(
                            "Added data volume '{}' as {} (state {}).",
                            device,
                            v["id"].as_str().unwrap_or("?"),
                            v["state"].as_str().unwrap_or("?")
                        );
                    } else {
                        let meta_lvs = parse_block_uri(&target, "sqmeta://")?;
                        let rec = squeezefs::config_ops::add_data_volume(
                            &meta_lvs,
                            &device,
                            no_rebalance,
                        )
                        .await?;
                        println!(
                            "Added data volume '{}' as {} (state {}).",
                            device, rec.id, rec.state
                        );
                    }
                    println!("{rebalance_note}");
                }
                VolumeActions::RemoveData {
                    target,
                    volume_id,
                    throttle,
                } => {
                    if live(&target) {
                        let body = admin_roundtrip(
                            &target,
                            "volume-remove-data",
                            &format!("{volume_id} {throttle}"),
                        )?;
                        let v: serde_json::Value = serde_json::from_str(&body)
                            .map_err(|e| format!("undecodable admin reply: {e}"))?;
                        println!(
                            "Volume '{volume_id}' is draining (evacuation job {}, throttle \
                             {throttle} %). It serves reads until retired; watch \
                             `squeezefs volume list {target}` / `squeezefs job list {target}`.",
                            v["job_id"].as_str().unwrap_or("?")
                        );
                    } else {
                        let meta_lvs = parse_block_uri(&target, "sqmeta://")?;
                        squeezefs::config_ops::remove_data_volume_offline(
                            &meta_lvs, &volume_id, throttle,
                        )
                        .await?;
                        println!("Volume '{volume_id}' evacuated and retired.");
                    }
                }
                VolumeActions::Undrain { target, volume_id } => {
                    if live(&target) {
                        admin_roundtrip(&target, "volume-undrain", &volume_id)?;
                    } else {
                        let meta_lvs = parse_block_uri(&target, "sqmeta://")?;
                        squeezefs::config_ops::undrain_data_volume(&meta_lvs, &volume_id).await?;
                    }
                    println!(
                        "Volume '{volume_id}' is active again (drain cancelled; already-moved \
                         blocks stay where the mover put them — CoW moves are never undone)."
                    );
                }
                VolumeActions::RepairSet { target } => {
                    if live(&target) {
                        return Err(
                            "volume repair-set is an OFFLINE verb: pass the sqmeta:// URI \
                             (it takes the single-writer claims like format-grade verbs; \
                             unmount first)"
                                .into(),
                        );
                    }
                    let meta_lvs = parse_block_uri(&target, "sqmeta://")?;
                    let restamped = squeezefs::config_ops::repair_meta_set(&meta_lvs).await?;
                    if restamped.is_empty() {
                        println!("Nothing to repair.");
                    } else {
                        println!(
                            "Re-stamped {} member volume(s); the set now mounts with \
                             order-independent discovery.",
                            restamped.len()
                        );
                    }
                }
                VolumeActions::AddMeta {
                    target,
                    device,
                    take_slots,
                } => {
                    if live(&target) {
                        return Err(
                            "volume add-meta is an OFFLINE verb (design-volume-lifecycle \
                             §5.5.2: the membership change runs a D0-guarded coordinator \
                             with the KD-8 staging drain barrier): unmount first and pass \
                             the sqmeta:// URI"
                                .into(),
                        );
                    }
                    let meta_lvs = parse_block_uri(&target, "sqmeta://")?;
                    let take =
                        if take_slots.contains(',') || take_slots.parse::<u32>().is_err() {
                            let mut slots = Vec::new();
                            for part in take_slots.split(',') {
                                slots.push(part.trim().parse::<u16>().map_err(|_| {
                                    format!("--take-slots: '{part}' is not a slot id")
                                })?);
                            }
                            squeezefs::config_ops::TakeSlots::List(slots)
                        } else {
                            squeezefs::config_ops::TakeSlots::Count(
                                take_slots.parse::<u32>().expect("checked above"),
                            )
                        };
                    let taken =
                        squeezefs::config_ops::add_meta_volume(&meta_lvs, &device, &take).await?;
                    println!(
                        "Added metadata volume '{device}' hosting slot(s) {taken:?}. Mount \
                         with the EXTENDED URI (all members listed); the old URI now \
                         refuses loud."
                    );
                }
                VolumeActions::MigrateMetaSlot {
                    target,
                    slot,
                    target_volume,
                } => {
                    if !live(&target) {
                        return Err("volume migrate-meta-slot runs the ONLINE engine on a live \
                             mount — pass the mountpoint (offline set changes are \
                             add-meta/remove-meta)"
                            .into());
                    }
                    let body = admin_roundtrip(
                        &target,
                        "migrate-meta-slot",
                        &format!("{slot} {target_volume}"),
                    )?;
                    let v: serde_json::Value = serde_json::from_str(&body)
                        .map_err(|e| format!("undecodable admin reply: {e}"))?;
                    println!(
                        "Slot {slot} migration to volume {target_volume} submitted (job {}). \
                         Watch `squeezefs job list {target}`; the cutover window rides \
                         `meta_slot_cutover_ms_max` on .stats.",
                        v["job_id"].as_str().unwrap_or("?")
                    );
                }
                VolumeActions::RemoveMeta { target, victim } => {
                    if live(&target) {
                        return Err(
                            "volume remove-meta is an OFFLINE verb (design-volume-lifecycle \
                             §5.5.2): unmount first and pass the sqmeta:// URI"
                                .into(),
                        );
                    }
                    let meta_lvs = parse_block_uri(&target, "sqmeta://")?;
                    squeezefs::config_ops::remove_meta_volume(&meta_lvs, &victim).await?;
                    println!(
                        "Removed metadata volume '{victim}' (slots migrated to the \
                         survivors; the victim carries a retirement tombstone). Mount with \
                         the SURVIVOR URI — listing the victim refuses loud."
                    );
                }
                VolumeActions::List { target, json } => {
                    let rows: serde_json::Value = if live(&target) {
                        serde_json::from_str(&admin_roundtrip(&target, "volume-list", "")?)
                            .map_err(|e| format!("undecodable admin reply: {e}"))?
                    } else {
                        let meta_lvs = parse_block_uri(&target, "sqmeta://")?;
                        let recs =
                            squeezefs::config_ops::resolved_volume_records(&meta_lvs).await?;
                        // Offline honesty: capacity comes from the device
                        // size where the device is reachable; used/free
                        // need the census walk (`squeezefs df`) — omitted
                        // rather than guessed (§5.2 lands the full math
                        // with VL4 preflight).
                        serde_json::Value::Array(
                            recs.iter()
                                .map(|r| {
                                    let capacity =
                                        squeezefs::nvme_dev::device_capacity_bytes(&r.backing_dev)
                                            .ok();
                                    serde_json::json!({
                                        "id": r.id,
                                        "backing_dev": r.backing_dev,
                                        "state": r.state,
                                        "added_ts": r.added_ts,
                                        "capacity_bytes": capacity,
                                    })
                                })
                                .collect(),
                        )
                    };
                    if json {
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                    } else {
                        println!(
                            "{:<22} {:<10} {:<8} {:>14} {:>14} BACKING",
                            "ID", "STATE", "HEALTHY", "CAPACITY", "USED"
                        );
                        let fmt_bytes = |v: &serde_json::Value| match v.as_u64() {
                            Some(b) => b.to_string(),
                            None => "-".to_string(),
                        };
                        for row in rows.as_array().cloned().unwrap_or_default() {
                            println!(
                                "{:<22} {:<10} {:<8} {:>14} {:>14} {}",
                                row["id"].as_str().unwrap_or("?"),
                                row["state"].as_str().unwrap_or("?"),
                                row["healthy"]
                                    .as_bool()
                                    .map(|h| h.to_string())
                                    .unwrap_or_else(|| "-".into()),
                                fmt_bytes(&row["capacity_bytes"]),
                                fmt_bytes(&row["used_bytes"]),
                                row["backing_dev"].as_str().unwrap_or("?"),
                            );
                        }
                    }
                }
            }
        }
        Commands::Job { action } => {
            let live = |t: &str| !t.starts_with("sqmeta://");
            match action {
                JobActions::List { target } => {
                    if live(&target) {
                        println!("{}", admin_roundtrip(&target, "job-list", "")?);
                    } else {
                        let recs = job_probe_records(&target).await?;
                        println!("{}", serde_json::to_string_pretty(&recs)?);
                    }
                }
                JobActions::Status { target, job_id } => {
                    if live(&target) {
                        println!("{}", admin_roundtrip(&target, "job-status", &job_id)?);
                    } else {
                        let recs = job_probe_records(&target).await?;
                        match recs.iter().find(|r| r.job_id == job_id) {
                            Some(r) => println!("{}", serde_json::to_string_pretty(r)?),
                            None => return Err(format!("unknown job {job_id}").into()),
                        }
                    }
                }
                JobActions::Pause { target, job_id }
                | JobActions::Resume { target, job_id }
                | JobActions::Cancel { target, job_id }
                    if !live(&target) =>
                {
                    let _ = job_id;
                    return Err("mutating job verbs need a live mount (offline probes are \
                                read-only — design-volume-lifecycle §6)"
                        .into());
                }
                JobActions::Pause { target, job_id } => {
                    println!("{}", admin_roundtrip(&target, "job-pause", &job_id)?);
                }
                JobActions::Resume { target, job_id } => {
                    println!("{}", admin_roundtrip(&target, "job-resume", &job_id)?);
                }
                JobActions::Cancel { target, job_id } => {
                    println!("{}", admin_roundtrip(&target, "job-cancel", &job_id)?);
                }
                JobActions::Throttle {
                    target,
                    job_id,
                    pct,
                } => {
                    if !live(&target) {
                        return Err("job throttle needs a live mount".into());
                    }
                    println!(
                        "{}",
                        admin_roundtrip(&target, "job-throttle", &format!("{job_id} {pct}"))?
                    );
                }
                JobActions::Worker { meta_uri } => {
                    run_job_worker(&meta_uri).await?;
                }
            }
        }
        Commands::Fsck {
            target,
            reports,
            online,
            offline,
            throttle,
            json,
            shards,
            scrub,
            repair,
            apply,
            quarantine_dir,
        } => {
            run_fsck_verb(
                &target,
                &reports,
                online,
                offline,
                throttle,
                json,
                shards,
                scrub,
                false,
                FsckRepairArgs {
                    repair,
                    apply,
                    quarantine_dir,
                },
            )
            .await?;
        }
        Commands::Scrub {
            target,
            throttle,
            json,
        } => {
            run_fsck_verb(
                &target,
                &[],
                false,
                false,
                throttle,
                json,
                None,
                true,
                true,
                FsckRepairArgs {
                    repair: false,
                    apply: false,
                    quarantine_dir: None,
                },
            )
            .await?;
        }
        Commands::Defrag {
            target,
            data,
            volume,
            meta,
            fold,
            rebalance,
            report_only,
            throttle,
            json,
        } => {
            run_defrag_verb(
                &target,
                DefragVerbArgs {
                    data,
                    volume,
                    meta,
                    fold,
                    rebalance,
                    report_only,
                    throttle,
                    json,
                },
            )
            .await?;
        }
        Commands::Claim { action } => {
            let ClaimActions::Clear { meta_uri } = action;
            squeezefs::set_fs_prefix("squeezefs");
            let meta_lvs = parse_block_uri(&meta_uri, "sqmeta://")?;
            let mut failures = 0usize;
            for path in &meta_lvs {
                match squeezefs::meta_backend::kv::backend::KvMetaBackend::claim_clear(
                    std::path::Path::new(path),
                )
                .await
                {
                    Ok(squeezefs::meta_backend::kv::backend::ClaimClearOutcome::NoClaim) => {
                        println!("{path}: no writer claim present — nothing to clear");
                    }
                    Ok(squeezefs::meta_backend::kv::backend::ClaimClearOutcome::Cleared(
                        holder,
                    )) => {
                        println!(
                            "{path}: cleared stale writer claim (holder id={}, pid={}, \
                             boot={}, last heartbeat ts={}). This was an operator \
                             attestation that the holder is down — if it was merely \
                             partitioned, stop it before it reconnects.",
                            holder.id, holder.pid, holder.boot, holder.ts
                        );
                    }
                    Err(e) => {
                        eprintln!("\x1b[91mERROR\x1b[0m {path}: {e}");
                        failures += 1;
                    }
                }
            }
            if failures > 0 {
                return Err(format!(
                    "claim clear refused/failed on {failures} of {} volume(s)",
                    meta_lvs.len()
                )
                .into());
            }
        }
        Commands::Mount {
            args,
            mem_cache_size,
            disk_cache_size,
            disk_cache_paths: _,
            data_lv: backing_dev,
            ip: _,
            port: _,
            subnqn: _,

            daemon: _,
            supervise: _,
            uid,
            gid,
            no_writeback,
            interception,
            allow_other,
            check_storage: _check_storage,
            options,
            read_cache_size,
            write_cache_size,
            read_mem_cache_size,
            write_mem_cache_size,
            mem_budget: _,
            meta_lv,
            dismount_wait,
            upload_delay,
            fuse_io_uring_sqpoll_idle_ms,
            fuse_io_uring_sqpoll_cpu,
            job_cpu_limit,
            write_verification: _,
            write_verification_sample: _,
            // VAL-3: consumed pre-fork in `main` (the material crossed
            // into this process in memory, never on argv); the resolution
            // itself happens against the volume's key reference.
            encrypt_key: _,
        } => {
            let (meta_lvs, mountpoint) = if let Some(ref m_lvs) = meta_lv {
                if args.is_empty() {
                    return Err("Error: Mountpoint path is required".into());
                }
                let m_point = PathBuf::from(&args[0]);
                (m_lvs.clone(), m_point)
            } else {
                if args.len() < 2 {
                    return Err(
                        "Error: Metadata URI (sqmeta://...) and Mountpoint path are required"
                            .into(),
                    );
                }
                let mut m_lvs = Vec::new();
                for i in 0..(args.len() - 1) {
                    m_lvs.extend(parse_block_uri(&args[i], "sqmeta://")?);
                }
                let m_point = PathBuf::from(&args[args.len() - 1]);
                (m_lvs, m_point)
            };

            // Version-gated bootstrap (PR K6a): root-inode presence and
            // the format config are read off the SLOT-0 HOST's KV trees
            // via a read-only probe mount (§5.5.1a discovery — PR VL5b:
            // after a slot-0 migration the root records live in a guest
            // keyspace on another member; legacy sets keep reading the
            // first volume's ino 1 verbatim).
            let disc = squeezefs::meta_backend::discover_meta_set(&meta_lvs).await?;
            let slot0_home = disc.ordered_paths[disc.slot_to_volume[0]].clone();
            let boot_vol = squeezefs::meta_backend::open_volume_probe(&slot0_home).await?;
            let boot_root = boot_vol.slot0_root_ino();
            let _root_inode = boot_vol.getattr(boot_root).await?;
            let val_opt = boot_vol
                .getxattr(
                    boot_root,
                    squeezefs::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
                )
                .await?;
            let val = val_opt.ok_or(
                "Format configuration xattr not found on root inode. Is this volume formatted?",
            )?;
            let format_config: FormatConfig = serde_json::from_slice(&val)?;
            drop(boot_vol);

            let resolved_mem_cache_size = mem_cache_size.unwrap_or_else(|| {
                format_config
                    .mem_cache_size
                    .clone()
                    .unwrap_or_else(|| "1GB".to_string())
            });
            let resolved_disk_cache_size = disk_cache_size.unwrap_or_else(|| {
                format_config
                    .disk_cache_size
                    .clone()
                    .unwrap_or_else(|| "10GB".to_string())
            });

            let resolved_read_cache_size = read_cache_size.unwrap_or_else(|| {
                format_config
                    .read_cache_size
                    .clone()
                    .unwrap_or_else(|| halve_size_string(&resolved_disk_cache_size, "5GB"))
            });
            let resolved_write_cache_size = write_cache_size.unwrap_or_else(|| {
                format_config
                    .write_cache_size
                    .clone()
                    .unwrap_or_else(|| halve_size_string(&resolved_disk_cache_size, "5GB"))
            });

            let resolved_read_mem_cache_size = read_mem_cache_size.unwrap_or_else(|| {
                format_config
                    .read_mem_cache_size
                    .clone()
                    .unwrap_or_else(|| halve_size_string(&resolved_mem_cache_size, "512MB"))
            });
            let resolved_write_mem_cache_size = write_mem_cache_size.unwrap_or_else(|| {
                format_config
                    .write_mem_cache_size
                    .clone()
                    .unwrap_or_else(|| halve_size_string(&resolved_mem_cache_size, "512MB"))
            });

            let resolved_dismount_wait: u64 = dismount_wait
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    format_config
                        .dismount_wait
                        .clone()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(10)
                });
            let resolved_upload_delay = upload_delay.unwrap_or_else(|| {
                format_config
                    .upload_delay
                    .clone()
                    .unwrap_or_else(|| "500ms".to_string())
            });

            squeezefs::cache::parse_duration(&resolved_upload_delay)?;

            let resolved_fuse_io_uring_sqpoll_idle_ms = fuse_io_uring_sqpoll_idle_ms
                .unwrap_or_else(|| format_config.fuse_io_uring_sqpoll_idle_ms.unwrap_or(0));
            let resolved_fuse_io_uring_sqpoll_cpu =
                resolve_local_sqpoll_cpu(fuse_io_uring_sqpoll_cpu);

            // Cache paths come from the format config ONLY — the mount
            // flag was rejected at CLI parse (cache-path policy). A format
            // that declared no paths runs permanently cache-less: the
            // isolation loop below is a no-op, `TieredCache` comes up with
            // no staging dirs, and the routing layer's layout gates
            // (`staging_dirs().is_empty()`) send every beyond-inline write
            // down the striped/inline paths instead of the staged tier.
            let staging_dirs = format_config.disk_cache_paths.clone().unwrap_or_default();

            let mut active_staging_dirs = staging_dirs;
            let fs_name = "squeezefs".to_string();

            let sanitized_mount = mountpoint
                .to_string_lossy()
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect::<String>();
            let mut sanitized_mount_clean = String::new();
            let mut last_was_underscore = false;
            for c in sanitized_mount.chars() {
                if c == '_' {
                    if !last_was_underscore {
                        sanitized_mount_clean.push(c);
                        last_was_underscore = true;
                    }
                } else {
                    sanitized_mount_clean.push(c);
                    last_was_underscore = false;
                }
            }
            let sanitized_mount_clean = sanitized_mount_clean.trim_matches('_');

            // TOCTOU backstop behind the bootstrap staging preflight: if a
            // root changed underneath us since the check, the failure still
            // names the directory and the chown remedy.
            let staging_remedy = |dir: &Path, e: std::io::Error| {
                let euid = unsafe { libc::geteuid() };
                let egid = unsafe { libc::getegid() };
                format!(
                    "failed to prepare staging dir '{}' as the daemon identity \
                     (uid {euid} gid {egid}): {e}; --uid/--gid are FUSE-presentation-only. \
                     Remedy: `sudo chown -R {euid}:{egid}` on the staging root, then retry \
                     the mount",
                    dir.display()
                )
            };
            let mut isolated_staging_dirs = Vec::new();
            for dir in active_staging_dirs {
                let isolated_dir = dir.join(&fs_name).join(sanitized_mount_clean);
                let shared_cache_dir = dir.join(&fs_name).join("cache_segment");

                fs::create_dir_all(&isolated_dir)
                    .await
                    .map_err(|e| staging_remedy(&isolated_dir, e))?;
                fs::create_dir_all(&shared_cache_dir)
                    .await
                    .map_err(|e| staging_remedy(&shared_cache_dir, e))?;

                let symlink_path = isolated_dir.join("cache_segment");
                let metadata = fs::symlink_metadata(&symlink_path).await;
                if let Ok(meta) = metadata {
                    if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                        fs::remove_dir_all(&symlink_path).await?;
                    } else {
                        fs::remove_file(&symlink_path).await?;
                    }
                }

                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink("../cache_segment", &symlink_path)?;
                }

                isolated_staging_dirs.push(isolated_dir);
            }
            active_staging_dirs = isolated_staging_dirs;

            // The durable volume set (KD-5): `data_volumes` records when
            // present, else legacy `data_lv` paths grandfathered with
            // basename ids (byte-identical registration names — every
            // historical `name://offset` block key keeps resolving). A
            // `--data-lv` mount override synthesizes legacy-shaped
            // records the same way.
            let volume_records: Vec<squeezefs::DataVolumeRecord> = match backing_dev {
                Some(paths) => paths
                    .iter()
                    .map(|p| squeezefs::DataVolumeRecord {
                        id: squeezefs::legacy_volume_id(p),
                        backing_dev: p.clone(),
                        state: squeezefs::VOL_STATE_ACTIVE.to_string(),
                        added_ts: 0,
                    })
                    .collect(),
                None => format_config.resolved_data_volumes(),
            };
            // VL4: retired records are id-tombstones (path cleared, kept
            // forever — KD-5); the mount registers `Active|Draining|
            // disabled` members only.
            let mount_records: Vec<squeezefs::DataVolumeRecord> = volume_records
                .iter()
                .filter(|r| r.state != squeezefs::VOL_STATE_RETIRED)
                .cloned()
                .collect();
            if mount_records.is_empty() {
                return Err("Error: no data volumes specified or configured".into());
            }
            let resolved_data_lvs: Vec<String> = mount_records
                .iter()
                .map(|r| r.backing_dev.clone())
                .collect();

            // Every data volume must be a usable backing device, not just the
            // first one — a typo'd second volume should fail here, not at
            // first I/O.
            for path in &resolved_data_lvs {
                squeezefs::storage::validate_backing_device(path)?;
            }
            let first_record = &mount_records[0];
            let first_data_path = &first_record.backing_dev;

            let dlm = DlmClient::new()?;

            let block_alloc = std::sync::Arc::new(
                squeezefs::block_allocator::BlockAllocator::new(&first_record.id).await?,
            );
            match squeezefs::nvme_dev::device_capacity_bytes(first_data_path) {
                Ok(cap) => block_alloc.set_capacity_bytes(cap),
                Err(e) => log::warn!(
                    "could not size data volume {first_data_path}: {e}; allocator unbounded"
                ),
            }

            // DUR-2 decision (a): the mount refuses a data volume that
            // cannot serve O_DIRECT (buffered device I/O has no barrier),
            // and logs the volume's probed volatile-write-cache class.
            let nvme_dev = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::open_checked(
                first_data_path,
            )?);

            // Filesystem generation of the mounted volume set (v3 superblock
            // uuids / v2 config identity, ordered): local staging is bound
            // to it, so a reformat that did not wipe the staging dirs can
            // never poison this mount with dead-generation segments.
            let fs_generation = squeezefs::meta_backend::volume_set_generation(&meta_lvs).await?;

            let cache = TieredCache::new(
                active_staging_dirs.clone(),
                Some(&resolved_read_mem_cache_size),
                Some(&resolved_write_mem_cache_size),
                Some(&resolved_read_cache_size),
                Some(&resolved_write_cache_size),
                block_alloc.clone(),
                nvme_dev.clone(),
                Some(&fs_generation),
            )
            .await?;

            let router = DataRouter::new(dlm.clone(), cache, block_alloc.clone(), nvme_dev.clone());

            // ONE registration path (design-volume-lifecycle §5.3 step 3):
            // mount and the online `volume add-data` share
            // `BackendRouter::register_backend`. The first record's
            // backing dev IS the router's default slot, so its build
            // reuses the default Arcs (bare-key invariant preserved);
            // durable `disabled` states land as health overrides.
            for rec in &mount_records {
                log::info!(
                    "Registering data volume '{}' at path {} (state {})",
                    rec.id,
                    rec.backing_dev,
                    rec.state
                );
                router.backend_router.register_backend(rec).await?;
            }
            // The record SNAPSHOT keeps the retired tombstones (KD-5:
            // `volume list` shows them; their ids can never be reused).
            router
                .backend_router
                .set_volume_records(volume_records.clone());

            // Mount every volume through the sector-0 version gate
            // (design-cow-kv-metadata §6.1): v3 mounts read-write (the
            // §4.4 commit pipeline + §4.6 checkpoint task are live);
            // blank and legacy-v2 volumes refuse loud. The whole set
            // opens through the D0 single-writer guard in set order
            // (design-metadata-throughput §5.0): a refusal on volume k
            // releases the guards taken on volumes 0..k and fails the
            // mount LOUD — the refusal text names the holder and the
            // remedy (dead-pid auto-reclaim / wait for the claim TTL /
            // `squeezefs claim clear`).
            // PR VL5a: the routed open runs the §5.5.1a stamp discovery
            // first — stamped sets mount in canonical member_position
            // order with their frozen routing width + slot map regardless
            // of URI order; legacy sets keep URI order verbatim.
            // Disagreements (torn epochs, missing members, duplicate
            // positions) refuse LOUD naming the volumes.
            let routed_meta_backend =
                match squeezefs::meta_backend::open_routed_meta_set(&meta_lvs).await {
                    Ok(routed) => routed,
                    Err(e) => {
                        eprintln!("\x1b[91mERROR\x1b[0m mount refused: {e}");
                        return Err(e.into());
                    }
                };
            for be in &routed_meta_backend.volumes {
                // §10 mount log: format version, ledger seq chosen,
                // replay entries/dropped/ms, free extents — plus BOTH
                // resolved-OQ-2 atomicity fields (the contract class and
                // the physical probe; the probe is informational — the
                // CoW contract holds by construction) — and the guard
                // guarantee class this volume actually mounted with.
                let physical =
                    squeezefs::meta_backend::atomicity::probe_meta_volume(be.device_path());
                be.set_atomicity_physical(physical);
                let stats = be.replay_stats();
                log::info!(
                    "meta volume {}: format=3 ledger_seq={} replay_entries={} \
                     replay_dropped_torn={} replay_ms={} free_extents={} next_ino={} \
                     meta_volume_atomicity={} meta_volume_atomicity_physical={} \
                     writer_guard_mode={}",
                    be.device_path().display(),
                    be.mounted_ledger().seq,
                    stats.entries,
                    stats.dropped_torn,
                    stats.replay_ms,
                    be.free_extents(),
                    be.next_ino(),
                    be.atomicity_contract(),
                    physical,
                    be.writer_guard_mode(),
                );
            }

            let resolved_uid = uid.unwrap_or_else(|| {
                std::env::var("SUDO_UID")
                    .ok()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or_else(|| unsafe { libc::getuid() })
            });
            let resolved_gid = gid.unwrap_or_else(|| {
                std::env::var("SUDO_GID")
                    .ok()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or_else(|| unsafe { libc::getgid() })
            });

            let mut fs_engine = SqueezefsFilesystem::new(router, dlm, resolved_uid, resolved_gid);
            fs_engine
                .router
                .set_meta_backend(routed_meta_backend.clone());

            // Block-allocator refcount recovery before serving FUSE: walk
            // each volume's live inode tree (no allocator seed / bitmap
            // reconcile — monotonic inos §4.8; A/B bitmap loaded at open).
            for kv in &routed_meta_backend.volumes {
                for entry in fs_engine.router.backend_router.backends.iter() {
                    let backend = entry.value();
                    log::info!("Running block allocator recovery for data volume...");
                    if let Err(e) = backend
                        .block_allocator
                        .recover_active_blocks_v3(kv, &fs_engine.router.backend_router)
                        .await
                    {
                        log::error!("Failed to recover block allocator: {:?}", e);
                    }
                }
            }
            fs_engine.meta_backend = Some(routed_meta_backend.clone());
            fs_engine.dismount_wait = resolved_dismount_wait;

            // VL2: the coordinator-side job fabric (durable records,
            // duty-cycle throttle, crash-resume adoption). Local pool
            // sized like the L4 service posture; `--job-cpu-limit` is
            // the default throttle for jobs submitted without one.
            let fabric_workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8)
                .div_euclid(4)
                .clamp(2, 8);
            // VL4: the mover context — the data router + this mount's
            // quiescence probe (RAM active buffers + staging records) —
            // arms the evacuate/rebalance job types on the fabric.
            // VL7: plus the D3 fold hook (defrag drives the W2 fold
            // machinery through it) and the continuous frag_* gauge
            // worker (§5.7 — D1/D3 on cadence; D2/D4 on measure).
            let mover_ctx = squeezefs::jobs::MoverCtx::new(
                fs_engine.router.clone(),
                fs_engine.mover_quiesce_probe(),
            )
            .with_fold(fs_engine.defrag_fold_hook());
            squeezefs::defrag::spawn_gauge_worker(fs_engine.router.clone());
            let fabric = squeezefs::jobs::JobFabric::start(
                routed_meta_backend,
                fabric_workers,
                job_cpu_limit,
                Some(mover_ctx),
            )
            .await
            .map_err(|e| format!("job fabric start failed: {e}"))?;
            fs_engine
                .job_fabric
                .store(std::sync::Arc::new(Some(fabric.clone())));

            // PR VL2b: the §5.1.6 job-shard execution wire — the
            // coordinator's TCP listener, the WERO fence over the data
            // namespaces, and the endpoint published through the
            // mount-registration heartbeat for worker discovery. PR VL4:
            // the PRODUCTION RouterShardDevice seam (allocation + block
            // I/O over the registered backends' io_uring workers); mover
            // job types themselves stay local-pool
            // (`JobType::wire_executable`).
            //
            // VAL-6: the posture is now OPERATOR-EXPRESSIBLE
            // (`SQUEEZEFS_JOB_WIRE_*` — bind/disable, CA-pinned mTLS,
            // verify sampling, connection cap, challenge freshness).
            // Unset ⇒ ruling D2's default verbatim: bind `0.0.0.0:0`,
            // plaintext, mandatory-100 % verify-reads.
            let wire_cfg = squeezefs::job_wire::JobWireConfig::from_env(
                resolved_data_lvs
                    .iter()
                    .map(std::path::PathBuf::from)
                    .collect(),
            )
            .map_err(|e| format!("job wire configuration refused: {e}"))?;
            let wire_seam = squeezefs::job_wire::RouterShardDevice::new(
                fs_engine.router.backend_router.clone(),
                format_config.block_size as usize,
            );
            let wire = squeezefs::job_wire::JobWireHost::start(fabric, wire_cfg, wire_seam)
                .await
                .map_err(|e| format!("job wire start failed: {e}"))?;
            if wire.listening() {
                let advertised = format!(
                    "{}:{}",
                    squeezefs::job_wire::local_advertise_ip(),
                    wire.endpoint().port()
                );
                let _ = fs_engine.job_wire_endpoint.set(advertised.clone());
                log::info!(
                    "job wire: endpoint {advertised} (published via the mount registration)"
                );
            }

            let opt_idle = if resolved_fuse_io_uring_sqpoll_idle_ms > 0 {
                Some(resolved_fuse_io_uring_sqpoll_idle_ms)
            } else {
                None
            };
            apply_fuse_io_uring_sqpoll_env(opt_idle, resolved_fuse_io_uring_sqpoll_cpu);

            println!("Mounting Squeezefs at {:?}...", mountpoint);

            let writeback_val = !no_writeback;

            // The CLI `--interception` flag rides the option string into
            // `start_mount` (where `resolve_interception_posture` folds
            // it with `-o interception` / SQUEEZEFS_IPC=1 and applies the
            // KD-11 write-through flip).
            let options = if interception {
                Some(match options {
                    Some(o) => format!("{o},interception"),
                    None => "interception".to_string(),
                })
            } else {
                options
            };

            start_mount(
                mountpoint,
                fs_engine,
                resolved_uid,
                resolved_gid,
                writeback_val,
                allow_other,
                options,
            )
            .await?;
        }
        Commands::Bench {
            mountpoint,
            write,
            read,
            stat,
            del,
            threads,
            files,
            size,
            block,
            rand,
            direct,
            time,
            iterations,
        } => {
            // ENG-8: `squeezefs bench` is the in-binary measurement path —
            // the one place a profiling build turns directly into a number.
            // Loud on stderr as well as the log: a bench row is captured
            // from the console far more often than from the daemon log.
            if let Some(warning) = squeezefs::version::profiling_build_warning(
                squeezefs::version::measurement_disqualifiers(),
            ) {
                eprintln!("WARNING: {warning}");
                log::warn!("{warning}");
            }
            let inv = squeezefs::bench::BenchInvocation {
                write,
                read,
                stat,
                del,
                threads,
                files,
                size,
                block,
                rand,
                direct,
                time,
                iterations,
            };
            squeezefs::bench::run_invocation(&mountpoint, &inv).await?;
        }
        Commands::Clone {
            meta_uri,
            src,
            dest,
        } => {
            squeezefs::set_fs_prefix("squeezefs");
            // The metadata set is NOT optional: both paths are resolved
            // through it, and the D0 single-writer guard is taken over it
            // (RW2 audit, docs/design-random-small-writes.md §5.1 — an
            // unguarded clone could pin nothing and alias blocks a live
            // daemon concurrently frees). Without it there is literally
            // nothing to clone, which is exactly how this verb spent its
            // life exiting 0 having done nothing.
            let uri = meta_uri.ok_or_else(|| {
                "Error: no metadata volume specified — pass sqmeta://<meta_dev>[,<meta_dev>…] \
                 (or set SQUEEZEFS_META_URI). `clone` resolves both paths through the \
                 metadata set and takes the single-writer guard over it."
                    .to_string()
            })?;
            let meta_lvs = parse_block_uri(&uri, "sqmeta://")?;
            println!(
                "Cloning file from {} to {} (metadata-only CoW)...",
                src, dest
            );
            squeezefs::config_ops::clone_path_offline(&meta_lvs, &src, &dest).await?;
            println!("File cloned successfully.");
        }
        Commands::Df {
            meta_uri,
            path,
            json,
        } => {
            squeezefs::set_fs_prefix("squeezefs");
            let target = meta_uri.or(path).ok_or_else(|| {
                "Error: no metadata volume specified — pass sqmeta://<meta_dev> (or a \
                 metadata volume path). A mounted filesystem also answers plain \
                 `df -h <mountpoint>`."
                    .to_string()
            })?;
            let meta_lvs = if target.starts_with("sqmeta://") {
                parse_block_uri(&target, "sqmeta://")?
            } else {
                vec![target]
            };
            run_df_report(&meta_lvs, json).await?;
        }
        Commands::Storage { action } => match action {
            StorageActions::Pool(pool_action) => match pool_action {
                StoragePoolActions::Create { pool_name, disks } => {
                    squeezefs::storage::pool_create(&pool_name, &disks)?;
                }
                StoragePoolActions::Add { pool_name, disks } => {
                    squeezefs::storage::pool_add(&pool_name, &disks)?;
                }
                StoragePoolActions::Remove {
                    pool_name,
                    disks,
                    yes,
                } => {
                    squeezefs::storage::pool_remove(&pool_name, &disks, yes)?;
                }
                StoragePoolActions::Delete { pool_name, yes } => {
                    squeezefs::storage::pool_delete(&pool_name, yes)?;
                }
                StoragePoolActions::List => {
                    squeezefs::storage::pool_list()?;
                }
            },
            StorageActions::Volume(vol_action) => match vol_action {
                StorageVolumeActions::Create {
                    pool_name,
                    vol_name,
                    size,
                    stripes,
                    stripe_size,
                } => {
                    squeezefs::storage::volume_create(
                        &pool_name,
                        &vol_name,
                        &size,
                        stripes,
                        stripe_size.as_deref(),
                    )?;
                }
                StorageVolumeActions::Extend {
                    pool_name,
                    vol_name,
                    add_size,
                } => {
                    squeezefs::storage::volume_extend(&pool_name, &vol_name, &add_size)?;
                }
                StorageVolumeActions::Delete {
                    pool_name,
                    vol_name,
                    yes,
                } => {
                    squeezefs::storage::volume_delete(&pool_name, &vol_name, yes)?;
                }
                StorageVolumeActions::List => {
                    squeezefs::storage::volume_list()?;
                }
            },
        },
        Commands::Nvmeof { action } => {
            // The NvmeofError Display strings ARE the designed loud-fail /
            // runbook UX (design §6.2/§6.3/§6.4) — render them verbatim,
            // never through Debug.
            if let Err(e) = dispatch_nvmeof(action) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Commands::Tune => {
            tune_system()?;
        }
        Commands::Config { meta_uri, action } => {
            squeezefs::set_fs_prefix("squeezefs");
            match action {
                ConfigActions::Set { key, value } => {
                    let _ = (key, value);
                    return Err(removed_verb(
                        "config set",
                        "it silently ignored every key and changed nothing.",
                        "squeezefs format (quotas are format-time until the \
                         volume-lifecycle capacity verbs land)",
                    ));
                }
                ConfigActions::SetCachePaths { uri, paths, force } => {
                    let meta_lvs = parse_block_uri(&uri, "sqmeta://")?;
                    squeezefs::config_ops::set_cache_paths(&meta_lvs, &paths, force)
                        .await
                        .map_err(|e| format!("cannot change cache paths: {}", e))?;
                    println!(
                        "Cache paths set to {} (new directories wiped; the next mount \
                         stamps a fresh staging generation).",
                        paths
                            .iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                ConfigActions::GetCachePaths { uri } => {
                    let meta_lvs = parse_block_uri(&uri, "sqmeta://")?;
                    match squeezefs::config_ops::get_cache_paths(&meta_lvs).await? {
                        Some(paths) if !paths.is_empty() => {
                            println!(
                                "{}",
                                paths
                                    .iter()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                        }
                        _ => println!("none (cache-less)"),
                    }
                }
                ConfigActions::DiskCache(action) => {
                    // Staging/cache paths are a format-time declaration
                    // with ONE real admin verb pair; the old `list`
                    // printed an always-empty ephemeral table off the
                    // deleted /dev/shm runtime config.
                    let (verb, why, successor) = match action {
                        DiskCacheActions::List => (
                            "config disk-cache list",
                            "it listed an always-empty ephemeral table (the /dev/shm \
                             runtime config is deleted; the durable truth is the format \
                             config).",
                            "squeezefs config get-cache-paths",
                        ),
                        DiskCacheActions::Add { .. } => (
                            "config disk-cache add",
                            "it was a silent no-op that changed nothing.",
                            "squeezefs config set-cache-paths",
                        ),
                        DiskCacheActions::Remove { .. } => (
                            "config disk-cache remove",
                            "it was a silent no-op that changed nothing.",
                            "squeezefs config set-cache-paths",
                        ),
                        DiskCacheActions::Enable { .. } => (
                            "config disk-cache enable",
                            "it was a silent no-op that changed nothing.",
                            "squeezefs config set-cache-paths",
                        ),
                        DiskCacheActions::Disable { .. } => (
                            "config disk-cache disable",
                            "it was a silent no-op that changed nothing.",
                            "squeezefs config set-cache-paths",
                        ),
                        DiskCacheActions::Flush { .. } => (
                            "config disk-cache flush",
                            "it was a silent no-op that changed nothing.",
                            "squeezefs config set-cache-paths",
                        ),
                    };
                    return Err(removed_verb(verb, why, successor));
                }
                ConfigActions::DataVolume(action) => match action {
                    // KEPT (real): the fail-stop health overrides + list,
                    // re-homed off the deleted /dev/shm runtime config
                    // (PR VL3): live mounts take the admin lane; a
                    // `-g sqmeta://` target flips DURABLE record state
                    // through the guarded offline path. `disable` routes
                    // NEW writes away; blocks already on the volume read
                    // EIO until re-enable — it is NOT an evacuation (that
                    // is `volume remove-data`, PR VL4).
                    DataVolumeActions::List => match resolve_config_target(&meta_uri)? {
                        ConfigTarget::Live(mnt) => {
                            let body = admin_roundtrip(&mnt, "volume-list", "")?;
                            let v: serde_json::Value = serde_json::from_str(&body)
                                .map_err(|e| format!("undecodable admin reply: {e}"))?;
                            println!("{}", serde_json::to_string_pretty(&v)?);
                        }
                        ConfigTarget::Offline(meta_lvs) => {
                            let recs =
                                squeezefs::config_ops::resolved_volume_records(&meta_lvs).await?;
                            println!("{}", serde_json::to_string_pretty(&recs)?);
                        }
                    },
                    DataVolumeActions::Enable { volume_id } => {
                        match resolve_config_target(&meta_uri)? {
                            ConfigTarget::Live(mnt) => {
                                println!("{}", admin_roundtrip(&mnt, "volume-enable", &volume_id)?);
                            }
                            ConfigTarget::Offline(meta_lvs) => {
                                squeezefs::config_ops::set_data_volume_state(
                                    &meta_lvs,
                                    &volume_id,
                                    squeezefs::VOL_STATE_ACTIVE,
                                )
                                .await?;
                                println!(
                                    "Data volume '{}' enabled (durable state; health override \
                                     cleared at next mount).",
                                    volume_id
                                );
                            }
                        }
                    }
                    DataVolumeActions::Disable { volume_id } => {
                        match resolve_config_target(&meta_uri)? {
                            ConfigTarget::Live(mnt) => {
                                println!(
                                    "{}",
                                    admin_roundtrip(&mnt, "volume-disable", &volume_id)?
                                );
                            }
                            ConfigTarget::Offline(meta_lvs) => {
                                squeezefs::config_ops::set_data_volume_state(
                                    &meta_lvs,
                                    &volume_id,
                                    squeezefs::VOL_STATE_DISABLED,
                                )
                                .await?;
                                println!(
                                    "Data volume '{}' disabled (durable fail-stop health \
                                     override: new writes fail over; blocks already on it read \
                                     EIO until re-enabled — this is not an evacuation).",
                                    volume_id
                                );
                            }
                        }
                    }
                    DataVolumeActions::Add { .. } => {
                        return Err(removed_verb(
                            "config data-volume add",
                            "it wrote only an ephemeral host-local file; the daemon never \
                             registered the volume and the durable set was untouched.",
                            "squeezefs volume add-data",
                        ));
                    }
                    DataVolumeActions::Remove { .. } => {
                        return Err(removed_verb(
                            "config data-volume remove",
                            "it removed nothing real (ephemeral bookkeeping only) and \
                             evacuated no data.",
                            "squeezefs volume remove-data",
                        ));
                    }
                    DataVolumeActions::Migrate { .. } => {
                        return Err(removed_verb(
                            "config data-volume migrate",
                            "it shelled out to LVM pvmove — not filesystem-block-aware, no \
                             refcount/fencing awareness, durable set untouched.",
                            "squeezefs volume remove-data (drain)",
                        ));
                    }
                },
                ConfigActions::MetadataVolume(action) => {
                    // Meta-volume health overrides are RUNTIME-ONLY (the
                    // admin lane on a live mount): no durable meta-volume
                    // records exist until the slot-map machinery
                    // (design-volume-lifecycle §5.5, PR VL5a) — offline
                    // targets refuse loud instead of faking durability.
                    let offline_refusal = |verb: &str| -> Box<dyn std::error::Error> {
                        format!(
                            "`squeezefs config metadata-volume {verb}` needs a live mount \
                             (admin lane): metadata-volume health overrides are runtime-only \
                             until durable meta-volume records land \
                             (design-volume-lifecycle §5.5, PR VL5a)."
                        )
                        .into()
                    };
                    match action {
                        // KEPT (real): fail-stop health overrides + list.
                        MetadataVolumeActions::List => match resolve_config_target(&meta_uri)? {
                            ConfigTarget::Live(mnt) => {
                                // The `.config` virtual file already
                                // publishes the live meta-volume table.
                                let raw = std::fs::read_to_string(
                                    std::path::Path::new(&mnt).join(".config"),
                                )
                                .map_err(|e| format!("cannot read {mnt}/.config: {e}"))?;
                                let v: serde_json::Value = serde_json::from_str(&raw)?;
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&v["metadata_volumes"])?
                                );
                            }
                            ConfigTarget::Offline(meta_lvs) => {
                                // Offline honesty: the durable truth is
                                // the volume set itself; no durable
                                // enable/disable state exists yet (VL5a).
                                let rows: Vec<serde_json::Value> = meta_lvs
                                    .iter()
                                    .enumerate()
                                    .map(|(idx, path)| {
                                        serde_json::json!({
                                            "id": format!("meta_volume_{idx}"),
                                            "backing_dev": path,
                                        })
                                    })
                                    .collect();
                                println!("{}", serde_json::to_string_pretty(&rows)?);
                            }
                        },
                        MetadataVolumeActions::Enable { volume_id } => {
                            match resolve_config_target(&meta_uri)? {
                                ConfigTarget::Live(mnt) => {
                                    println!(
                                        "{}",
                                        admin_roundtrip(&mnt, "meta-volume-enable", &volume_id)?
                                    );
                                }
                                ConfigTarget::Offline(_) => return Err(offline_refusal("enable")),
                            }
                        }
                        MetadataVolumeActions::Disable { volume_id } => {
                            match resolve_config_target(&meta_uri)? {
                                ConfigTarget::Live(mnt) => {
                                    println!(
                                        "{}",
                                        admin_roundtrip(&mnt, "meta-volume-disable", &volume_id)?
                                    );
                                }
                                ConfigTarget::Offline(_) => return Err(offline_refusal("disable")),
                            }
                        }
                        MetadataVolumeActions::Add { .. } => {
                            return Err(removed_verb(
                                "config metadata-volume add",
                                "it wrote only an ephemeral host-local file; the meta volume \
                                 set is fixed at format until the slot-map machinery lands.",
                                "squeezefs volume add-meta",
                            ));
                        }
                        MetadataVolumeActions::Remove { .. } => {
                            return Err(removed_verb(
                                "config metadata-volume remove",
                                "it removed nothing real and migrated no metadata.",
                                "squeezefs volume remove-meta",
                            ));
                        }
                        MetadataVolumeActions::Migrate { .. } => {
                            return Err(removed_verb(
                                "config metadata-volume migrate",
                                "it moved NO data — route bookkeeping only, by its own \
                                 admission.",
                                "squeezefs volume remove-meta (slot migration)",
                            ));
                        }
                    }
                }
                ConfigActions::List => {
                    return Err(removed_verb(
                        "config list",
                        "it printed the ephemeral /dev/shm runtime config (deleted in the \
                         volume-lifecycle re-home; the durable truth lives on the volumes and \
                         the live truth in the daemon).",
                        "squeezefs volume list / config data-volume list / config \
                         get-cache-paths / .config on a live mount",
                    ));
                }
                ConfigActions::Fsck => {
                    return Err(removed_verb(
                        "config fsck",
                        "it checked NOTHING (an empty stub that always reported clean — \
                         worse than no fsck).",
                        "squeezefs fsck",
                    ));
                }
            }
        }
        Commands::Umount {
            meta_uri: _,
            mountpoint,
            force,
        } => {
            let _ctrl_c_guard = spawn_ctrl_c_handler("unmount");
            use std::io::IsTerminal;
            use std::io::Write;

            squeezefs::set_fs_prefix("squeezefs");

            // Staged/active-write visibility is DAEMON-AUTHORITATIVE: read
            // the mounted filesystem's virtual `.stats` file. This CLI must
            // never open or map the daemon's live staging segment files —
            // the historical implementation constructed a second `NvmeCache`
            // over `staging_segment/` with a hardcoded 100 MiB budget, whose
            // `set_len` truncated the live daemon's 128 MiB mmaps to
            // 6.25 MiB/shard: every teardown-drain access beyond the new EOF
            // then died with SIGBUS mid-flush and the truncation destroyed
            // staged payload bytes (FIND-VS-A, the vs-JuiceFS scoreboard's
            // teardown-SIGBUS class; evidence in
            // `.benchmarks/2026-07-16-find-vs-a-fix.md`).
            fn read_mount_staging_stats(mountpoint: &Path) -> Option<(usize, usize, u64)> {
                let stats_str = std::fs::read_to_string(mountpoint.join(".stats")).ok()?;
                let v: serde_json::Value = serde_json::from_str(&stats_str).ok()?;
                let staged = v["nvme_staged_write_file_ids"]
                    .as_array()
                    .map(|a| a.len())
                    .unwrap_or(0);
                let active = v["active_writes"]
                    .as_object()
                    .map(|m| {
                        m.values()
                            .map(|blocks| blocks.as_array().map(|a| a.len()).unwrap_or(0))
                            .sum()
                    })
                    .unwrap_or(0);
                let bytes = v["metrics"]["nvme_staging_current_bytes"]
                    .as_u64()
                    .unwrap_or(0);
                Some((staged, active, bytes))
            }

            // Resolve dismount_wait limit (default: 10) and find active daemon PID
            let dismount_wait = 10;
            let mut daemon_pid: Option<u32> = None;

            let initial_stats = read_mount_staging_stats(&mountpoint);
            let (staged_count, active_writes_count, total_bytes_at_start) =
                initial_stats.unwrap_or((0, 0, 0));

            let has_unflushed = staged_count > 0 || active_writes_count > 0;
            let mut choice = "continue"; // default non-interactive behavior

            // Prompt the user if not forced and stdin is a TTY
            if has_unflushed && !force && std::io::stdin().is_terminal() {
                println!(
                    "{}",
                    "WARNING: There are unflushed staged writes on this node!"
                        .red()
                        .bold()
                );
                println!("Remaining local staged files: {}", staged_count);
                println!(
                    "Active write transaction directories: {}",
                    active_writes_count
                );
                println!("Other nodes will NOT see this data if you unmount now.");
                println!("\nChoose an option:");
                println!(
                    "  [w] Wait for staged files to drain/flush to NVMe-oF backend (recommended)"
                );
                println!("  [c] Continue unmount now (staged data stays on disk; the next mount recovers it)");
                println!("  [a] Abort unmount");
                print!("Select option [w/c/a]: ");
                let _ = std::io::stdout().flush();

                let mut input = String::new();
                if std::io::stdin().read_line(&mut input).is_ok() {
                    let trimmed = input.trim().to_lowercase();
                    if trimmed == "w" || trimmed == "wait" {
                        choice = "wait";
                    } else if trimmed == "c" || trimmed == "continue" {
                        choice = "continue";
                    } else {
                        choice = "abort";
                    }
                } else {
                    choice = "abort";
                }
            }

            match choice {
                "abort" => {
                    println!("Unmount aborted.");
                    return Ok(());
                }
                "wait" => {
                    println!("Waiting for staged writes to drain (limit: {}s). Press 's' and Enter to skip wait.", dismount_wait);
                    let start_wait = std::time::Instant::now();
                    let max_wait = std::time::Duration::from_secs(dismount_wait);
                    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
                    tokio::spawn(async move {
                        let mut input = String::new();
                        while std::io::stdin().read_line(&mut input).is_ok() {
                            let trimmed = input.trim().to_lowercase().to_string();
                            let _ = tx.send(trimmed).await;
                            input.clear();
                        }
                    });

                    let mut skipped = false;
                    let mut current_staged = staged_count;
                    let mut current_active = active_writes_count;

                    loop {
                        while let Ok(msg) = rx.try_recv() {
                            if msg == "s" || msg == "skip" {
                                skipped = true;
                                break;
                            }
                        }
                        if skipped {
                            break;
                        }

                        let current_bytes = match read_mount_staging_stats(&mountpoint) {
                            Some((s, a, b)) => {
                                current_staged = s;
                                current_active = a;
                                b
                            }
                            None => {
                                // Mount gone / daemon unreachable: nothing
                                // left to poll — proceed with the unmount.
                                break;
                            }
                        };

                        if current_staged == 0 && current_active == 0 {
                            println!("\nAll staged files and active writes drained cleanly!");
                            break;
                        }

                        let elapsed = start_wait.elapsed().as_secs_f64();
                        let bytes_flushed = total_bytes_at_start.saturating_sub(current_bytes);
                        let speed = if elapsed > 0.1 {
                            bytes_flushed as f64 / elapsed
                        } else {
                            0.0
                        };
                        let speed_mb = speed / (1024.0 * 1024.0);

                        let progress_pct = if total_bytes_at_start > 0 {
                            100.0 * bytes_flushed as f64 / total_bytes_at_start as f64
                        } else {
                            100.0
                        };

                        print!(
                            "\rProgress: {:.1}% | Remaining: {} files, {:.2} MB | Speed: {:.2} MB/s | Elapsed: {}s / Limit: {}s (Press 's' to skip)",
                            progress_pct,
                            current_staged,
                            current_bytes as f64 / 1024.0 / 1024.0,
                            speed_mb,
                            elapsed.round(),
                            dismount_wait
                        );
                        let _ = std::io::stdout().flush();

                        if start_wait.elapsed() >= max_wait {
                            println!("\nWait limit expired.");
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }

                    if skipped || current_staged > 0 || current_active > 0 {
                        if skipped {
                            println!("\nWait skipped by user.");
                        }
                        // The daemon owns every staged byte (never-lossy
                        // teardown): its shutdown drain keeps flushing after
                        // the unmount signal, and anything it cannot finish
                        // is recovered by the next mount\'s staging recovery
                        // (`recover_staging` + generation gate). This CLI
                        // deliberately has NO discard path — mutating the
                        // live daemon\'s segment files from a second process
                        // was the FIND-VS-A teardown-SIGBUS vector.
                        println!(
                            "Unmount will continue; the daemon\'s teardown drain flushes what it \
                             can and the next mount recovers the rest."
                        );
                    }
                }
                _ => {
                    if has_unflushed {
                        println!("Continuing with unmount despite unflushed data.");
                    }
                }
            }

            // 7. Execute the unmount (prefer a *real* unmount; lazy only with --force)
            let abs_mp = std::fs::canonicalize(&mountpoint).unwrap_or_else(|_| mountpoint.clone());
            let mp_str = abs_mp.to_string_lossy().to_string();

            // Prefer a live daemon from /proc. Garnet active_clients can be stale
            // (old PID after crash / kill -9), which would skip wait/kill incorrectly.
            match find_squeezefs_daemon_pid(&abs_mp) {
                Some(pid) => daemon_pid = Some(pid),
                None => {
                    if let Some(pid) = daemon_pid {
                        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                            daemon_pid = None;
                        }
                    }
                }
            }

            if !is_path_mounted(&abs_mp) {
                println!("Mountpoint {:?} is not currently mounted.", abs_mp);
                if let Some(pid) = daemon_pid {
                    eprintln!(
                        "However, a squeezefs daemon (PID {}) is still running for this path.",
                        pid
                    );
                    if force {
                        eprintln!("--force: sending SIGTERM then SIGKILL to PID {}...", pid);
                        let _ = nix_kill(pid, false);
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                            let _ = nix_kill(pid, true);
                        }
                    } else {
                        eprintln!(
                            "Re-run with --force to kill the orphaned daemon, or: sudo kill {pid}"
                        );
                        std::process::exit(1);
                    }
                }
                return Ok(());
            }

            // Show openers so a busy mount is diagnosable without loof guesswork.
            let holders = find_mount_holders(&abs_mp);
            if !holders.is_empty() {
                println!(
                    "Processes currently using {} (will block a non-lazy umount):",
                    mp_str
                );
                for line in &holders {
                    println!("  {}", line);
                }
            }

            println!("Unmounting squeezefs at {}...", mp_str);

            let mut unmount_success = false;

            if let Some(pid) = daemon_pid {
                println!("Sending SIGTERM to squeezefs daemon (PID {})...", pid);
                if nix_kill(pid, false).is_ok() {
                    // Wait for daemon to exit and unmount itself
                    let start_wait = std::time::Instant::now();
                    let max_wait = std::time::Duration::from_secs(dismount_wait.max(5));
                    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                        if start_wait.elapsed() >= max_wait {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                        unmount_success = true;
                    } else {
                        println!("Daemon (PID {}) did not exit within timeout.", pid);
                    }
                }
            }

            if !unmount_success {
                println!("Daemon unmount failed or no daemon found. Attempting direct unmount...");
                unmount_success = try_clean_unmount(&abs_mp);
            }

            if !unmount_success && force {
                // --force: kill user-space holders (not the fuse daemon — kernel abort
                // should stop it), retry clean/-f, and only then lazy-detach.
                eprintln!(
                    "--force: clean umount failed; killing non-daemon holders and retrying..."
                );
                for line in &holders {
                    if let Some(pid) = line
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u32>().ok())
                    {
                        if Some(pid) != daemon_pid {
                            let _ = nix_kill(pid, false);
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                unmount_success = try_clean_unmount(&abs_mp);
                if !unmount_success {
                    eprintln!(
                        "--force: still busy after umount -f; lazy unmount (umount -l) as last resort"
                    );
                    unmount_success = std::process::Command::new("umount")
                        .args(["-l", &mp_str])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                }
            }

            if unmount_success {
                println!("Successfully unmounted mountpoint {}", mp_str);
                // Kernel abort should make the over-uring pool shut down and the
                // daemon exit on its own. Wait, then only kill leftovers with --force.
                if let Some(pid) = daemon_pid {
                    let proc_path = format!("/proc/{pid}");
                    if std::path::Path::new(&proc_path).exists() {
                        print!(
                            "Waiting for FUSE daemon (PID {}) to exit after kernel abort...",
                            pid
                        );
                        let _ = std::io::stdout().flush();
                        let start_wait = std::time::Instant::now();
                        // Kernel uring teardown can be async; give it a few seconds.
                        let max_wait = std::time::Duration::from_secs(dismount_wait.max(5));
                        while std::path::Path::new(&proc_path).exists() {
                            if start_wait.elapsed() >= max_wait {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        if !std::path::Path::new(&proc_path).exists() {
                            println!(" done.");
                        } else if force {
                            println!();
                            eprintln!(
                                "Daemon PID {} still alive after unmount; --force: SIGTERM/SIGKILL...",
                                pid
                            );
                            let _ = nix_kill(pid, false);
                            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                            if std::path::Path::new(&proc_path).exists() {
                                let _ = nix_kill(pid, true);
                            }
                            if std::path::Path::new(&proc_path).exists() {
                                eprintln!("Warning: failed to kill daemon PID {}", pid);
                            } else {
                                println!("Daemon killed.");
                            }
                        } else {
                            println!();
                            eprintln!(
                                "Warning: daemon PID {} still running after unmount (should self-exit on abort).",
                                pid
                            );
                            eprintln!(
                                "This is a bug if it persists; re-run with --force to kill it, or: sudo kill {}",
                                pid
                            );
                        }
                    }
                }
            } else {
                eprintln!("Error: Failed to unmount {}.", mp_str);
                if holders.is_empty() {
                    eprintln!(
                        "No obvious user-space holders found. If this persists after a rebuild with the fuse-over-uring abort fix, try:"
                    );
                    eprintln!("  sudo fuser -vm {}", mp_str);
                    eprintln!("  sudo loof --dir-tree {}", mp_str);
                } else {
                    eprintln!(
                        "Stop the processes listed above (or cd out of the mount), then retry."
                    );
                }
                eprintln!(
                    "Try: sudo umount -f {0}   or   sudo squeezefs umount --force {0}",
                    mp_str
                );
                eprintln!(
                    "Lazy unmount only if nothing else works: sudo umount -l {}",
                    mp_str
                );
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// True if `path` is a mount point according to `/proc/self/mountinfo`.
fn is_path_mounted(path: &std::path::Path) -> bool {
    let want = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => path.to_path_buf(),
    };
    let want_s = want.to_string_lossy();
    let want_trim = want_s.trim_end_matches('/');
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        // Fall back to findmnt / stat FS type if mountinfo unreadable.
        return std::process::Command::new("findmnt")
            .args(["-n", "-T"])
            .arg(path)
            .output()
            .ok()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                o.status.success() && s.contains("fuse")
            })
            .unwrap_or(false);
    };
    for line in mountinfo.lines() {
        // mountinfo: … mountpoint … - fstype …
        let mut parts = line.split(" - ");
        let left = parts.next().unwrap_or("");
        let fields: Vec<&str> = left.split_whitespace().collect();
        // field 5 is mount point (may be octal-escaped)
        if fields.len() < 5 {
            continue;
        }
        let mp = fields[4].replace("\\040", " ").replace("\\011", "\t");
        let mp_trim = mp.trim_end_matches('/');
        if mp_trim == want_trim {
            return true;
        }
    }
    false
}

/// Best-effort list of processes with cwd or open FDs under `mountpoint`.
/// Lines look like: `1234 cwd=/mnt/foo cmd=bash`
fn find_mount_holders(mountpoint: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let want = match std::fs::canonicalize(mountpoint) {
        Ok(p) => p,
        Err(_) => mountpoint.to_path_buf(),
    };
    let want_s = want.to_string_lossy();
    let prefix = format!("{}/", want_s.trim_end_matches('/'));
    let root = want_s.trim_end_matches('/').to_string();

    let Ok(proc) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in proc.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid: u32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let proc_path = entry.path();
        let cmdline = std::fs::read(proc_path.join("cmdline"))
            .ok()
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .replace('\0', " ")
                    .trim()
                    .chars()
                    .take(80)
                    .collect::<String>()
            })
            .unwrap_or_default();

        // cwd
        if let Ok(cwd) = std::fs::read_link(proc_path.join("cwd")) {
            let c = cwd.to_string_lossy();
            if c == root || c.starts_with(&prefix) {
                out.push(format!("{pid} cwd={c} cmd={cmdline}"));
                continue;
            }
        }

        // open fds
        let Ok(fd_dir) = std::fs::read_dir(proc_path.join("fd")) else {
            continue;
        };
        for fd in fd_dir.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                let t = target.to_string_lossy();
                if t == root || t.starts_with(&prefix) {
                    out.push(format!(
                        "{pid} fd={} -> {t} cmd={cmdline}",
                        fd.file_name().to_string_lossy()
                    ));
                    break;
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Locate a live `squeezefs mount … <mountpoint>` process.
fn find_squeezefs_daemon_pid(mountpoint: &std::path::Path) -> Option<u32> {
    let want = match std::fs::canonicalize(mountpoint) {
        Ok(p) => p,
        Err(_) => mountpoint.to_path_buf(),
    };
    let want_s = want.to_string_lossy().trim_end_matches('/').to_string();
    let Ok(proc) = std::fs::read_dir("/proc") else {
        return None;
    };
    for entry in proc.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(mut bytes) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        let args: Vec<String> = bytes
            .split(|&b| b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let is_mount =
            args.iter().any(|a| a.contains("squeezefs")) && args.iter().any(|a| a == "mount");
        if !is_mount {
            continue;
        }
        for arg in &args {
            if !arg.starts_with('/') {
                continue;
            }
            let norm = arg.trim_end_matches('/');
            let matches = if let Ok(abs) = std::path::Path::new(arg).canonicalize() {
                abs.to_string_lossy().trim_end_matches('/') == want_s
            } else {
                norm == want_s
            };
            if matches {
                return Some(pid);
            }
        }
    }
    None
}

/// Unmount without lazy detach.
///
/// Order: fusermount helpers → plain `umount` → `umount -f`.
/// After heavy fuse-over-uring workloads a single stranded kernel request can make
/// plain `umount` report EBUSY with **no** process holders; `-f` aborts the fuse
/// connection (daemon should self-exit). Lazy (`-l`) is never used here.
fn try_clean_unmount(mountpoint: &std::path::Path) -> bool {
    let mp = mountpoint.as_os_str();
    let attempts: [(&str, Vec<&std::ffi::OsStr>); 4] = [
        ("fusermount3", vec![std::ffi::OsStr::new("-u"), mp]),
        ("fusermount", vec![std::ffi::OsStr::new("-u"), mp]),
        ("umount", vec![mp]),
        ("umount", vec![std::ffi::OsStr::new("-f"), mp]),
    ];
    for (bin, args) in attempts {
        // Suppress noise from attempts that fail before a later one succeeds.
        let status = std::process::Command::new(bin)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(status, Ok(s) if s.success()) {
            return true;
        }
    }
    false
}

/// Send SIGTERM (or SIGKILL if `force_kill`).
fn nix_kill(pid: u32, force_kill: bool) -> std::io::Result<()> {
    let sig = if force_kill {
        libc::SIGKILL
    } else {
        libc::SIGTERM
    };
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn find_uri_from_proc(target_path: &std::path::Path) -> Option<String> {
    let target_abs = target_path
        .canonicalize()
        .unwrap_or_else(|_| target_path.to_path_buf());
    let norm_target = target_abs
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    let dir = std::fs::read_dir("/proc").ok()?;
    for entry in dir.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.chars().all(|c| c.is_ascii_digit()) {
                let cmdline_path = path.join("cmdline");
                if let Ok(mut bytes) = std::fs::read(cmdline_path) {
                    if bytes.is_empty() {
                        continue;
                    }
                    if bytes.last() == Some(&0) {
                        bytes.pop();
                    }
                    let args: Vec<String> = bytes
                        .split(|&b| b == 0)
                        .map(|slice| String::from_utf8_lossy(slice).into_owned())
                        .collect();
                    if args.iter().any(|arg| arg.contains("squeezefs"))
                        && args.iter().any(|arg| arg == "mount")
                    {
                        let mut found_uri = None;
                        let mut found_mountpoint = None;
                        for arg in &args {
                            if arg.starts_with("squeeze://") || arg.starts_with("redis://") {
                                found_uri = Some(arg.clone());
                            } else if arg.starts_with("/") {
                                let arg_path = std::path::Path::new(arg);
                                let norm_arg = arg.trim_end_matches('/');
                                let matches = if let Ok(abs_arg) = arg_path.canonicalize() {
                                    let norm_abs_arg =
                                        abs_arg.to_string_lossy().trim_end_matches('/').to_string();
                                    norm_target == norm_abs_arg
                                        || norm_target.starts_with(&format!("{}/", norm_abs_arg))
                                } else {
                                    norm_target == norm_arg
                                        || norm_target.starts_with(&format!("{}/", norm_arg))
                                };
                                if matches {
                                    found_mountpoint = Some(std::path::PathBuf::from(norm_arg));
                                }
                            }
                        }
                        if let (Some(uri), Some(_)) = (found_uri, found_mountpoint) {
                            return Some(uri);
                        }
                    }
                }
            }
        }
    }
    None
}

fn is_squeezefs_mount(path: &std::path::Path) -> bool {
    if path.join(".stats").exists() && path.join(".config").exists() {
        return true;
    }
    find_uri_from_proc(path).is_some()
}

fn find_squeezefs_mounts() -> Vec<PathBuf> {
    let mut mounts = Vec::new();
    if let Ok(content) = std::fs::read_to_string("/proc/mounts") {
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let mnt_dir = parts[1];
                let mnt_type = parts[2];
                if mnt_type.starts_with("fuse") {
                    let path = PathBuf::from(mnt_dir);
                    if is_squeezefs_mount(&path) {
                        mounts.push(path);
                    }
                }
            }
        }
    }
    mounts
}

/// `squeezefs clients` — list the mount registrations (client heartbeats +
/// the single-writer claim) recorded on the volume set's root inos,
/// classified under the existing staleness law (live / stale / dead-pid).
/// Read-only probes (the `status` access pattern): never blocked by, and
/// never perturbing, a live mount.
async fn run_clients_report(
    meta_lvs: &[String],
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut rows: Vec<(
        String,
        squeezefs::meta_backend::kv::backend::MountRegistration,
    )> = Vec::new();
    for path in meta_lvs {
        let be = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(
            std::path::Path::new(path),
        )
        .await
        .map_err(|e| format!("cannot probe metadata volume '{path}': {e}"))?;
        for reg in be.mount_registrations().await {
            rows.push((path.clone(), reg));
        }
    }

    let count_state = |s: &str| rows.iter().filter(|(_, r)| r.state() == s).count();
    let (live, stale, dead) = (
        count_state("live"),
        count_state("stale"),
        count_state("dead"),
    );

    if json {
        let clients: Vec<serde_json::Value> = rows
            .iter()
            .map(|(path, r)| {
                let mut v = r.to_json();
                v["volume"] = serde_json::Value::String(path.clone());
                v
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "volumes": meta_lvs,
                "clients": clients,
                "live": live,
                "stale": stale,
                "dead": dead,
            }))?
        );
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "No client registrations on {} metadata volume(s) — no mount (live or crashed) \
             holds this filesystem.",
            meta_lvs.len()
        );
        return Ok(());
    }
    println!(
        "{:<7} {:<38} {:<8} {:<6} {:<5} VOLUME",
        "KIND", "ID", "PID", "STATE", "AGE"
    );
    for (path, r) in &rows {
        println!(
            "{:<7} {:<38} {:<8} {:<6} {:<5} {}",
            r.kind,
            r.id,
            r.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            r.state(),
            r.age_secs
                .map(|a| format!("{a}s"))
                .unwrap_or_else(|| "-".into()),
            path,
        );
    }
    println!(
        "{} registration(s): {live} live, {stale} stale, {dead} dead (reclaimable).",
        rows.len()
    );
    Ok(())
}

/// `squeezefs df` — offline space/inode accounting for a volume set, from
/// the same authoritative sources the mounted daemon's honest statfs
/// serves: the formatted capacity/quotas (format config on the meta
/// volume), the striped-block allocator accounting (rebuilt by the same
/// inode-tree walk the mount runs), and the v3 monotonic ino watermark.
/// Meta volumes are opened with read-only probes (the `status` access
/// pattern) — works without a live mount and answers the durable
/// point-in-time state beside one.
async fn run_df_report(meta_lvs: &[String], json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut probes = Vec::new();
    for path in meta_lvs {
        let be = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(
            std::path::Path::new(path),
        )
        .await
        .map_err(|e| format!("cannot probe metadata volume '{path}': {e}"))?;
        probes.push((path.clone(), be));
    }

    // The format config lives on the root ino of the first volume (the
    // mount-time bootstrap contract).
    let raw = probes[0]
        .1
        .getxattr(1, squeezefs::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
        .await?
        .ok_or_else(|| {
            format!(
                "metadata volume '{}' carries no format config — volume not formatted by \
                 `squeezefs format`",
                probes[0].0
            )
        })?;
    let config: FormatConfig = serde_json::from_slice(&raw)
        .map_err(|e| format!("invalid format config on '{}': {e}", probes[0].0))?;

    // Retired records are id-tombstones (path cleared) — reported but
    // never registered/sized (VL4).
    let volume_records: Vec<squeezefs::DataVolumeRecord> = config
        .resolved_data_volumes()
        .into_iter()
        .filter(|r| r.state != squeezefs::VOL_STATE_RETIRED)
        .collect();
    if volume_records.is_empty() {
        return Err("format config names no data volumes".into());
    }

    // Rebuild the striped-block allocator accounting exactly the way a
    // mount does (one allocator per data volume record, refcounts
    // recovered by the live-inode-tree walk) — the authoritative usage
    // source behind statfs. Reads only; nothing is written. Registration
    // rides the durable ids (KD-5): a set with `vol-` members resolves
    // its `vol-…://offset` keys correctly here too.
    let first_record = &volume_records[0];
    let default_alloc = std::sync::Arc::new(
        squeezefs::block_allocator::BlockAllocator::new(&first_record.id).await?,
    );
    let default_dev = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        &first_record.backing_dev,
    ));
    let block_size = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(config.block_size));
    let backend_router = squeezefs::routing::BackendRouter::new(
        default_alloc.clone(),
        default_dev.clone(),
        block_size,
    );
    let mut volume_rows: Vec<(String, String, u64)> = Vec::new(); // (name, path, size)
    for rec in &volume_records {
        let size = squeezefs::nvme_dev::device_capacity_bytes(&rec.backing_dev)
            .map_err(|e| format!("cannot size data volume '{}': {e}", rec.backing_dev))?;
        let backend = backend_router.register_backend(rec).await?;
        backend.block_allocator.set_capacity_bytes(size);
        volume_rows.push((rec.id.clone(), rec.backing_dev.clone(), size));
    }
    backend_router.set_volume_records(volume_records.clone());
    for (path, kv) in &probes {
        for entry in backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(kv, &backend_router)
                .await
                .map_err(|e| {
                    format!("allocator accounting walk failed on meta volume '{path}': {e}")
                })?;
        }
    }

    // Aggregate numbers — the statfs semantics verbatim: total = the
    // formatted capacity (quota-aware), used = allocated striped-block
    // bytes, files = the inode quota vs the LIVE inode population
    // (POSIX-1: `live_inodes` counts every allocation cursor — the native
    // watermark AND the mint-spread guest cursors — so `df -i` and
    // `statfs` cannot disagree).
    let capacity = config.capacity;
    let used = backend_router.allocated_bytes();
    let free = capacity.saturating_sub(used);
    let inodes_total = config.inodes;
    let inodes_used: u64 = probes
        .iter()
        .map(|(_, v)| v.live_inodes())
        .sum::<u64>()
        .saturating_add(1); // the root inode itself
    let inodes_free = inodes_total.saturating_sub(inodes_used);

    let data_volumes: Vec<serde_json::Value> = volume_rows
        .iter()
        .map(|(name, path, size)| {
            let allocated = backend_router
                .backends
                .get(name)
                .map(|be| {
                    be.block_allocator
                        .get_used_blocks()
                        .saturating_mul(be.block_allocator.chunk_size())
                })
                .unwrap_or(0);
            serde_json::json!({
                "path": path,
                "size_bytes": size,
                "allocated_bytes": allocated,
            })
        })
        .collect();

    let mut meta_volumes = Vec::new();
    for (path, be) in &probes {
        let size = squeezefs::nvme_dev::device_capacity_bytes(path).unwrap_or(0);
        let sb = be.superblock();
        let node_size = sb.node_size as u64;
        let heap = sb.heap.len;
        let heap_free = be.free_extents().saturating_mul(node_size);
        meta_volumes.push(serde_json::json!({
            "path": path,
            "size_bytes": size,
            "kv_heap_bytes": heap,
            "kv_heap_free_bytes": heap_free,
            "kv_heap_used_bytes": heap.saturating_sub(heap_free),
            "next_ino": be.next_ino(),
        }));
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "name": config.name,
                "block_size": config.block_size,
                "capacity_bytes": capacity,
                "used_bytes": used,
                "free_bytes": free,
                "inodes": {
                    "total": inodes_total,
                    "used": inodes_used,
                    "free": inodes_free,
                },
                "data_volumes": data_volumes,
                "meta_volumes": meta_volumes,
            }))?
        );
        return Ok(());
    }

    let pct = if capacity > 0 {
        (used as f64 / capacity as f64) * 100.0
    } else {
        0.0
    };
    println!(
        "SqueezeFS '{}' — offline query over {} meta / {} data volume(s), durable state",
        config.name,
        meta_lvs.len(),
        volume_records.len()
    );
    println!(
        "Data:   capacity {}   used {} ({pct:.1}%)   free {}",
        format_size_human(capacity),
        format_size_human(used),
        format_size_human(free),
    );
    println!("Inodes: quota {inodes_total}   used {inodes_used}   free {inodes_free}");
    println!();
    println!("{:<52} {:>12} {:>12}", "DATA VOLUME", "SIZE", "ALLOCATED");
    for dv in &data_volumes {
        println!(
            "{:<52} {:>12} {:>12}",
            dv["path"].as_str().unwrap_or("?"),
            format_size_human(dv["size_bytes"].as_u64().unwrap_or(0)),
            format_size_human(dv["allocated_bytes"].as_u64().unwrap_or(0)),
        );
    }
    println!();
    println!(
        "{:<52} {:>12} {:>12} {:>12} {:>9}",
        "META VOLUME", "SIZE", "KV HEAP", "HEAP FREE", "NEXT-INO"
    );
    for mv in &meta_volumes {
        println!(
            "{:<52} {:>12} {:>12} {:>12} {:>9}",
            mv["path"].as_str().unwrap_or("?"),
            format_size_human(mv["size_bytes"].as_u64().unwrap_or(0)),
            format_size_human(mv["kv_heap_bytes"].as_u64().unwrap_or(0)),
            format_size_human(mv["kv_heap_free_bytes"].as_u64().unwrap_or(0)),
            mv["next_ino"].as_u64().unwrap_or(0),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Formatted capacity is bounded by physical backing — oversubscription
    /// is refused at the filesystem level (thin-provision underneath at the
    /// LVM/fabric layer if desired). Undersubscription (testing) and inode
    /// control remain supported.
    #[test]
    fn test_resolve_format_capacity_no_oversubscription() {
        const G400: u64 = 400 * 1024 * 1024 * 1024;
        const G100: u64 = 100 * 1024 * 1024 * 1024;
        const P1: u64 = 1024 * 1024 * 1024 * 1024 * 1024;

        // Default: exactly the physical total.
        assert_eq!(resolve_format_capacity(G400, None).unwrap(), G400);
        // Undersubscribe for testing: honored.
        assert_eq!(resolve_format_capacity(G400, Some(G100)).unwrap(), G100);
        // Equal is the boundary and is fine.
        assert_eq!(resolve_format_capacity(G400, Some(G400)).unwrap(), G400);

        // Oversubscribe: refused, naming both numbers.
        let err = resolve_format_capacity(G400, Some(P1)).unwrap_err();
        assert!(
            err.contains("oversubscription") || err.contains("exceeds"),
            "must name the refusal: {err}"
        );
        assert!(err.contains("400"), "must show the physical bound: {err}");

        // Unknown physical capacity: refuse loudly (the silent legacy
        // fallback fabricated 1 PiB out of thin air).
        assert!(resolve_format_capacity(0, None).is_err());
        assert!(resolve_format_capacity(0, Some(G100)).is_err());
        // Zero request is nonsense.
        assert!(resolve_format_capacity(G400, Some(0)).is_err());
    }

    #[test]
    fn test_parse_block_uri_valid() {
        let paths = parse_block_uri("sqmeta:///dev/vg/meta1,/dev/vg/meta2", "sqmeta://").unwrap();
        assert_eq!(paths, vec!["/dev/vg/meta1", "/dev/vg/meta2"]);

        let paths_data =
            parse_block_uri("sqdata://dev/vg/data1,dev/vg/data2", "sqdata://").unwrap();
        assert_eq!(paths_data, vec!["/dev/vg/data1", "/dev/vg/data2"]);
    }

    #[test]
    fn test_parse_block_uri_invalid() {
        assert!(parse_block_uri("http://127.0.0.1", "sqmeta://").is_err());
        assert!(parse_block_uri("sqmeta://", "sqmeta://").is_err());
    }
}

/// Auto-tune client node configurations (dirty page ratios, socket buffers, ulimit limits, and FUSE background connections)
pub fn tune_system() -> Result<(), std::io::Error> {
    println!(
        "{}",
        "=== Squeezefs Client Node Auto-Tuning ===".bold().cyan()
    );

    let is_root = unsafe { libc::getuid() } == 0;
    if !is_root {
        println!(
            "{}",
            "WARNING: Not running as root (sudo). Tuning parameters will be checked but cannot be applied.".yellow()
        );
    }

    // 1. Virtual Memory (dirty page ratios)
    let dr_path = "/proc/sys/vm/dirty_ratio";
    let dbg_path = "/proc/sys/vm/dirty_background_ratio";
    if let Ok(curr) = std::fs::read_to_string(dr_path) {
        println!("vm.dirty_ratio: current = {}", curr.trim());
    }
    if let Ok(curr) = std::fs::read_to_string(dbg_path) {
        println!("vm.dirty_background_ratio: current = {}", curr.trim());
    }
    if is_root {
        println!(
            "Applying optimized VM dirty ratios (dirty_ratio = 40, dirty_background_ratio = 10)..."
        );
        let _ = std::fs::write(dr_path, "40\n");
        let _ = std::fs::write(dbg_path, "10\n");
    }

    // 2. Network socket buffer limits
    let rmem_path = "/proc/sys/net/core/rmem_max";
    let wmem_path = "/proc/sys/net/core/wmem_max";
    if let Ok(curr) = std::fs::read_to_string(rmem_path) {
        println!("net.core.rmem_max: current = {} bytes", curr.trim());
    }
    if let Ok(curr) = std::fs::read_to_string(wmem_path) {
        println!("net.core.wmem_max: current = {} bytes", curr.trim());
    }
    if is_root {
        println!(
            "Optimizing net.core socket buffers (rmem_max = 67108864, wmem_max = 67108864)..."
        );
        let _ = std::fs::write(rmem_path, "67108864\n");
        let _ = std::fs::write(wmem_path, "67108864\n");
    }

    // 3. FUSE Connection parameters
    let fuse_conn_dir = "/sys/fs/fuse/connections";
    if std::path::Path::new(fuse_conn_dir).exists() {
        if let Ok(entries) = std::fs::read_dir(fuse_conn_dir) {
            for entry in entries.flatten() {
                let conn_path = entry.path();
                let max_bg_path = conn_path.join("max_background");
                let cong_path = conn_path.join("congestion_threshold");
                if max_bg_path.exists() {
                    let current: u64 = std::fs::read_to_string(&max_bg_path)
                        .ok()
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    println!(
                        "FUSE Connection {:?}: max_background = {}",
                        entry.file_name(),
                        current
                    );
                    if is_root && current < 256 {
                        // Raise-only floor (L1 policy; derivation sweep
                        // 2026-08-04): mounts negotiate max_background =
                        // ring capacity at INIT (often > 256 now) —
                        // `tune` must never LOWER a live connection, so
                        // it only lifts pre-L1-class values (< 256) to
                        // the 256/192 measured class.
                        let _ = std::fs::write(max_bg_path, "256\n");
                        let _ = std::fs::write(cong_path, "192\n");
                    }
                }

                if let Some(conn_id_str) = entry.file_name().to_str() {
                    let bdi_path_str = format!("/sys/class/bdi/0:{}/read_ahead_kb", conn_id_str);
                    let bdi_path = std::path::Path::new(&bdi_path_str);
                    if bdi_path.exists() {
                        if let Ok(curr) = std::fs::read_to_string(bdi_path) {
                            println!(
                                "FUSE Connection {}: read_ahead_kb = {}",
                                conn_id_str,
                                curr.trim()
                            );
                        }
                        if is_root {
                            println!(
                                "Applying optimized read_ahead_kb = 0 (disabled) for connection {}...",
                                conn_id_str
                            );
                            let _ = std::fs::write(bdi_path, "0\n");
                        }
                    }
                }
            }
        }
    }

    println!("=== Auto-tuning completed ===\n");
    Ok(())
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn spawn_ctrl_c_handler(action_name: &'static str) -> AbortOnDrop {
    let handle = tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!(
                    "\nWARNING: Ctrl+C pressed! If you really want to cancel {}, hit Ctrl+C again.",
                    action_name
                );
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("\n{} cancelled by user. Exiting...", action_name);
                        std::process::exit(130);
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                        eprintln!("\nCancel timeout elapsed. Resuming...");
                    }
                }
            }
        }
    });
    AbortOnDrop(handle)
}
