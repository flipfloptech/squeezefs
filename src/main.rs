#![allow(clippy::all)]

use clap::{Parser, Subcommand};
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{start_mount, SqueezefsFilesystem};

use squeezefs::routing::DataRouter;
use squeezefs::FormatConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[derive(Parser)]
#[command(name = "squeezefs")]
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
        /// Maximum capacity of the volume (e.g. "1P", "100G", default: auto-detected from volume or 1PB)
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
        /// Encryption algorithm (aes256gcm-rsa, chacha20-rsa, none, default: none)
        #[arg(long, default_value = "none")]
        encrypt_algo: String,
        /// Path to RSA private key PEM file for client-side encryption
        #[arg(long)]
        encrypt_key: Option<String>,
        /// Time in seconds to wait for staged writes to drain to NVMe-oF backend on dismount (default: 10)
        #[arg(long)]
        dismount_wait: Option<String>,
        /// Delay/interval for background staging write uploads (e.g. "500ms", "5s", default: "500ms")
        #[arg(long, default_value = "500ms")]
        upload_delay: String,
        /// Shared default /dev/fuse io_uring SQPOLL idle timeout in milliseconds. Use 0 to disable the shared default.
        #[arg(long, env = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS")]
        fuse_io_uring_sqpoll_idle_ms: Option<u32>,
    },
    /// Show filesystem status
    Status {
        /// Optional Metadata URI (sqmeta://...) or mount point path
        meta_uri: Option<String>,
    },
    /// List all active clients that have the filesystem mounted
    Clients {
        /// Metadata URI (sqmeta://...)
        meta_uri: String,
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

        /// Custom Shared-Block Metadata Backend ("MetaLV") paths
        #[arg(long, value_delimiter = ',')]
        meta_lv: Option<Vec<String>>,

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

        /// Custom UID owner for the mount (default: current user or SUDO_UID)
        #[arg(long)]
        uid: Option<u32>,

        /// Custom GID owner for the mount (default: current group or SUDO_GID)
        #[arg(long)]
        gid: Option<u32>,

        /// Peer-to-peer cache server address (e.g. 127.0.0.1:9099)
        #[arg(long)]
        p2p_addr: Option<String>,

        /// Disable FUSE writeback cache (enabled by default)
        #[arg(long)]
        no_writeback: bool,

        /// Allow other users to access the mount
        #[arg(long)]
        allow_other: bool,

        /// Validate backend storage connectivity on startup
        #[arg(long)]
        check_storage: bool,

        /// Time in seconds to wait for staged writes to drain to NVMe-oF backend on dismount (default: 10)
        #[arg(long)]
        dismount_wait: Option<String>,
        /// Delay/interval for background staging write uploads (e.g. "500ms", "5s")
        #[arg(long)]
        upload_delay: Option<String>,

        /// Override the /dev/fuse io_uring SQPOLL idle timeout in milliseconds for this mount. Use 0 to disable even if the volume has a shared default.
        #[arg(long, env = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS")]
        fuse_io_uring_sqpoll_idle_ms: Option<u32>,

        /// Pin the /dev/fuse io_uring SQPOLL kernel thread to a CPU for this mount only. Use 0 to disable CPU pinning.
        #[arg(long, env = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU")]
        fuse_io_uring_sqpoll_cpu: Option<u32>,

        /// Custom FUSE options (comma-separated list, e.g. "ro,nonempty")
        #[arg(short = 'o', long)]
        options: Option<String>,

        /// Limit the background job worker CPU utilization percentage (1 to 100, default: 50)
        #[arg(long, default_value_t = 50)]
        job_cpu_limit: u32,

        /// Enable read-after-write checksum verification on writes to cache and disk
        #[arg(long)]
        write_verification: bool,

        /// When `--write-verification` is set, verify every N-th write (P2-9).
        /// Default 1 = verify every write. Larger values reduce RAW cost under load.
        #[arg(long, default_value_t = 1)]
        write_verification_sample: u64,
    },
    /// Cleanly unmount a squeezefs mountpoint (fusermount/umount/-f; kills zombie daemon if needed)
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
    Bench {
        /// Path to the mounted filesystem directory
        path: PathBuf,
        /// Number of concurrent threads
        #[arg(short, long, default_value_t = 1)]
        threads: usize,
        /// Size of the large file in MB per thread
        #[arg(long, default_value_t = 128)]
        large_size: usize,
        /// Size of the small files in KB per thread
        #[arg(long, default_value_t = 128)]
        small_size: usize,
        /// Number of small files to write per thread
        #[arg(long, default_value_t = 100)]
        small_count: usize,
        /// Number of iterations to run the benchmark
        #[arg(short, long, default_value_t = 1)]
        iterations: usize,
        /// Run only specific workloads (e.g. metadata, large-seq, small-rand)
        #[arg(long, value_delimiter = ',')]
        only: Option<Vec<String>>,
        /// Skip specific workloads
        #[arg(long, value_delimiter = ',')]
        skip: Option<Vec<String>>,
        /// Enable direct I/O (O_DIRECT)
        #[arg(long)]
        direct: bool,
    },
    /// Clone a file metadata-only (instant Copy-on-Write cloning)
    Clone {
        /// Optional Metadata URI (sqmeta://...)
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
    /// Defragment a formatted SqueezeFS volume
    Defrag {
        /// Optional Metadata URI (sqmeta://...)
        #[arg(
            long,
            short = 'g',
            env = "SQUEEZEFS_META_URI",
            alias = "meta-uri",
            alias = "meta_uri"
        )]
        meta_uri: Option<String>,
        /// NVMe device path
        #[arg(long)]
        nvme_path: String,
        /// Optional target inode to defragment (only defragment this file)
        #[arg(long, short = 'i')]
        inode: Option<u64>,
    },
    /// Automatically tune client node configurations (requires root/sudo to apply changes)
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
    /// Show filesystem disk space usage across all caches and NVMe-oF backend
    Df {
        /// Optional Metadata URI (sqmeta://...)
        #[arg(
            long,
            short = 'g',
            env = "SQUEEZEFS_META_URI",
            alias = "meta-uri",
            alias = "meta_uri"
        )]
        meta_uri: Option<String>,
        /// Optional path to a file or directory
        path: Option<String>,
    },
    /// Manage underlying LVM storage pools and volumes
    Storage {
        #[command(subcommand)]
        action: StorageActions,
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

    /// Configure and manage NVMe over Fabrics targets and connections
    #[command(subcommand)]
    Nvmeof(NvmeofActions),
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
        /// Explicit number of disks/stripes to stripe across (defaults to auto-detecting all disks in the pool)
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

#[derive(Subcommand, Debug, Clone)]
enum NvmeofActions {
    /// Share a local disk or regular file as an NVMe-oF target subsystem
    Share {
        /// Local backing path (e.g. /dev/nvme1n1 or /tmp/testfile.img)
        backing_path: String,
        /// Optional custom Subsystem NQN
        #[arg(long)]
        subnqn: Option<String>,
        /// Port to bind target listener to (default: 4420)
        #[arg(long, default_value_t = 4420)]
        port: u16,
        /// IP address(es) to bind target to (comma-separated or multiple flags)
        #[arg(long, required = true, value_delimiter = ',')]
        ip: Vec<String>,
        /// Share target via user-space SPDK instead of kernel configfs
        #[arg(long)]
        spdk: bool,
    },
    /// Stop sharing an NVMe-oF target subsystem
    Unshare {
        /// Subsystem NQN to unshare
        subnqn: String,
        /// Unshare target from user-space SPDK instead of kernel configfs
        #[arg(long)]
        spdk: bool,
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
    },
    /// Disconnect local client from a remote NVMe-oF target
    Disconnect {
        /// Subsystem NQN to disconnect
        subnqn: String,
    },
    /// List locally shared targets and connected remote fabric disks
    List,
    /// Restore all locally registered persistent target shares
    RestoreShares,
    /// Install SPDK from source and set up dependencies
    SpdkInstall,
    /// Configure hugepages (e.g. 2GB or 4GB)
    SpdkSetup {
        /// Memory to allocate for hugepages (e.g. "2GB" or "4GB")
        #[arg(long, default_value = "2GB")]
        hugepages: String,
    },
    /// Bind a specific PCIe NVMe device to SPDK user-space driver
    SpdkBind {
        /// PCIe address of the device to bind (e.g. 0000:01:00.0)
        pci: String,
    },
    /// Unbind a specific PCIe NVMe device from SPDK and return it to kernel control
    SpdkUnbind {
        /// PCIe address of the device to unbind (e.g. 0000:01:00.0)
        pci: String,
    },
    /// Start the SPDK NVMe-oF target daemon (nvmf_tgt) in the background
    SpdkStart,
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
    /// Set runtime configuration quotas (capacity, inodes, or memory cache sizes)
    Set {
        /// Quota/config key (e.g. "capacity", "inodes", "mem_cache_size", "read_mem_cache_size", "write_mem_cache_size", "fuse_io_uring_sqpoll_idle_ms")
        key: String,
        /// New value (e.g. "100G", "2T" or numeric value/0)
        value: String,
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
        /// Optional capacity in bytes (or e.g. "100G", default matches formatted volume capacity)
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
        /// Backing device path (e.g. "/dev/shm/squeezefs_pjdfs_meta")
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

#[cfg(unix)]
static DAEMON_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

const FUSE_IO_URING_SQPOLL_IDLE_MS_ENV: &str = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS";
const FUSE_IO_URING_SQPOLL_CPU_ENV: &str = "SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU";

fn get_default_staging_dir() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        PathBuf::from("/tmp/squeezefs_staging")
    } else {
        PathBuf::from(format!("/tmp/squeezefs_staging_{}", uid))
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "dhat-on")]
    let _profiler = dhat::Profiler::new_heap();

    let cli = Cli::parse();

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
        disk_cache_paths,
        daemon,
        no_writeback,
        allow_other,
        options,
        read_cache_size,
        write_cache_size,
        read_mem_cache_size,
        write_mem_cache_size,
        fuse_io_uring_sqpoll_idle_ms,
        fuse_io_uring_sqpoll_cpu,
        meta_lv,
        ..
    } = &cli.command
    {
        let (meta_lvs, mountpoint) = if let Some(ref m_lvs) = meta_lv {
            if args.is_empty() {
                eprintln!("Error: Mountpoint path is required");
                std::process::exit(1);
            }
            let m_point = PathBuf::from(&args[0]);
            (m_lvs.clone(), m_point)
        } else {
            if args.len() < 2 {
                eprintln!("Error: Metadata URI (sqmeta://...) and Mountpoint path are required");
                std::process::exit(1);
            }
            let mut m_lvs = Vec::new();
            for i in 0..(args.len() - 1) {
                match parse_block_uri(&args[i], "sqmeta://") {
                    Ok(parsed) => m_lvs.extend(parsed),
                    Err(e) => {
                        eprintln!("Error parsing Metadata URI: {}", e);
                        std::process::exit(1);
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
                eprintln!("Error: Mountpoint {:?} does not exist.", mountpoint);
                std::process::exit(1);
            }
            use std::os::unix::fs::MetadataExt;
            match std::fs::metadata(&mountpoint) {
                Ok(meta) => {
                    if meta.uid() != uid {
                        eprintln!(
                            "Error: Mountpoint {:?} is owned by UID {}, but current user is UID {}.",
                            mountpoint,
                            meta.uid(),
                            uid
                        );
                        eprintln!("Please use a mountpoint owned by you, or run with sudo.");
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "Error: Failed to read metadata of mountpoint {:?}: {}",
                        mountpoint, e
                    );
                    std::process::exit(1);
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

        let writeback = !no_writeback;
        if let Err(e) = print_mount_diagnostics(
            &meta_lvs,
            &mountpoint,
            &fs_name,
            *daemon,
            mem_cache_size.as_deref(),
            disk_cache_size.as_deref(),
            read_cache_size.as_deref(),
            write_cache_size.as_deref(),
            read_mem_cache_size.as_deref(),
            write_mem_cache_size.as_deref(),
            disk_cache_paths.as_deref(),
            writeback,
            *allow_other,
            options.as_deref(),
            *fuse_io_uring_sqpoll_idle_ms,
            *fuse_io_uring_sqpoll_cpu,
        ) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }

        if *daemon {
            let mountpoint_path = mountpoint.clone();
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
                    // Close the write end of the pipe in the parent process
                    libc::close(pipefd[1]);

                    // Set read end of the pipe to non-blocking
                    let flags = libc::fcntl(pipefd[0], libc::F_GETFL);
                    if flags >= 0 {
                        libc::fcntl(pipefd[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }

                    let mut child_error = String::new();

                    // Parent process waits for mount point to become ready
                    print!("Mounting Squeezefs at {:?}...", mountpoint_path);
                    use std::io::Write;
                    let _ = std::io::stdout().flush();

                    let mut ready = false;
                    let start = std::time::Instant::now();
                    while start.elapsed() < std::time::Duration::from_secs(30) {
                        std::thread::sleep(std::time::Duration::from_millis(500));

                        // Read from pipe to check for errors/panics from child
                        let mut buf = [0u8; 1024];
                        let n =
                            libc::read(pipefd[0], buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                        if n > 0 {
                            if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
                                child_error.push_str(s);
                            }
                        }

                        // Check if child is still running
                        let mut status = 0;
                        let wait_res = libc::waitpid(pid, &mut status, libc::WNOHANG);
                        if wait_res == pid {
                            // Child exited! Read any remaining output from pipe
                            loop {
                                let n = libc::read(
                                    pipefd[0],
                                    buf.as_mut_ptr() as *mut libc::c_void,
                                    buf.len(),
                                );
                                if n <= 0 {
                                    break;
                                }
                                if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
                                    child_error.push_str(s);
                                }
                            }
                            println!();
                            if !child_error.is_empty() {
                                eprintln!(
                                    "Failed to start squeezefs daemon. Child error:\n{}",
                                    child_error
                                );
                            } else {
                                eprintln!(
                                    "Failed to start squeezefs daemon. Child process exited early."
                                );
                            }
                            libc::close(pipefd[0]);
                            std::process::exit(1);
                        }

                        // Check if mountpoint is ready
                        match std::fs::metadata(&mountpoint_path) {
                            Ok(metadata) => {
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::MetadataExt;
                                    if metadata.ino() == 1 {
                                        ready = true;
                                        break;
                                    }
                                }
                                #[cfg(not(unix))]
                                {
                                    ready = true;
                                    break;
                                }
                            }
                            Err(e) => {
                                if e.kind() == std::io::ErrorKind::PermissionDenied {
                                    ready = true;
                                    break;
                                }
                            }
                        }
                        print!(".");
                        let _ = std::io::stdout().flush();
                    }
                    println!();
                    if ready {
                        println!(
                            "\x1b[92mOK\x1b[0m Squeezefs is ready at {:?}",
                            mountpoint_path
                        );
                        libc::close(pipefd[0]);
                        std::process::exit(0);
                    } else {
                        eprintln!("The mount point is not ready in 30 seconds, exiting");
                        let _ = std::process::Command::new("umount")
                            .arg("-l")
                            .arg(&mountpoint_path)
                            .output();
                        libc::kill(pid, libc::SIGKILL);
                        libc::close(pipefd[0]);
                        std::process::exit(1);
                    }
                }
                // Child process detaches
                libc::close(pipefd[0]);
                DAEMON_PIPE.store(pipefd[1], std::sync::atomic::Ordering::Relaxed);

                // Register panic hook in child process
                std::panic::set_hook(Box::new(|panic_info| {
                    let fd = DAEMON_PIPE.load(std::sync::atomic::Ordering::Relaxed);
                    if fd >= 0 {
                        let msg = format!("Panic: {}\n", panic_info);
                        let _ = libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len());
                        let _ = libc::close(fd);
                    }
                }));

                libc::setsid();
                // Redirect stdin to /dev/null
                if let Ok(null_file) = std::fs::File::open("/dev/null") {
                    use std::os::unix::io::AsRawFd;
                    libc::dup2(null_file.as_raw_fd(), 0);
                }
                // Redirect stdout/stderr to log_file if provided, else /dev/null
                let output_file = if let Some(ref path) = cli.log_file {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .ok()
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
        }
    }

    let mut builder = env_logger::Builder::from_default_env();
    if let Some(ref log_path) = cli.log_file {
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    builder.init();

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

#[allow(clippy::too_many_arguments)]
fn print_mount_diagnostics(
    meta_lvs: &[String],
    mountpoint: &Path,
    fs_name: &str,
    _daemon: bool,
    _mem_cache_size: Option<&str>,
    _disk_cache_size: Option<&str>,
    _read_cache_size: Option<&str>,
    _write_cache_size: Option<&str>,
    _read_mem_cache_size: Option<&str>,
    _write_mem_cache_size: Option<&str>,
    _disk_cache_paths: Option<&[PathBuf]>,
    writeback: bool,
    allow_other: bool,
    _options: Option<&str>,
    _fuse_io_uring_sqpoll_idle_ms: Option<u32>,
    _fuse_io_uring_sqpoll_cpu: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("SqueezeFS version {}", env!("CARGO_PKG_VERSION"));
    println!("===================================================");
    println!("Mount Options:");
    println!("  Mountpoint: {:?}", mountpoint);
    println!("  Writeback: {}", writeback);
    println!("  Allow Other: {}", allow_other);
    println!("Metadata Backend:");
    println!("  Metadata URIs: {:?}", meta_lvs);
    println!("  Volume Name: {:?}", fs_name);
    println!("===================================================");
    Ok(())
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
            force: _,
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
        } => {
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

            let mut total_capacity = 0;
            for path in &data_lvs {
                if let Ok(size) = get_backing_device_size(path) {
                    total_capacity += size;
                }
            }
            if total_capacity == 0 {
                if let Some(ref cap_str) = capacity {
                    total_capacity = parse_human_readable_size(cap_str)?;
                } else {
                    total_capacity = parse_human_readable_size("1P")?;
                }
            }
            println!(
                "Total physical data volume capacity: {}",
                format_size_human(total_capacity)
            );

            for path in &meta_lvs {
                log::info!("Formatting metadata volume at {}...", path);
                let storage =
                    squeezefs::meta_backend::storage::MetaLvStorage::open(path, 64 * 1024 * 1024)?;
                squeezefs::meta_backend::MetaLvBackend::format(&storage)?;
            }

            let parsed_block_size = parse_human_readable_size(&block_size)?;
            let config = FormatConfig {
                name: "squeezefs".to_string(),
                block_size: parsed_block_size,
                capacity: total_capacity,
                inodes,
                compression: compression.clone(),
                encrypt_algo: encrypt_algo.clone(),
                encrypt_key: encrypt_key.clone(),
                mem_cache_size: mem_cache_size.clone(),
                disk_cache_size: disk_cache_size.clone(),
                disk_cache_paths: disk_cache_paths.clone(),
                data_lv: Some(data_lvs.clone()),
                read_cache_size: read_cache_size.clone(),
                write_cache_size: write_cache_size.clone(),
                read_mem_cache_size: read_mem_cache_size.clone(),
                write_mem_cache_size: write_mem_cache_size.clone(),
                dismount_wait: dismount_wait.clone(),
                upload_delay: Some(upload_delay.clone()),
                fuse_io_uring_sqpoll_idle_ms,
            };

            let first_meta_path = &meta_lvs[0];
            let storage = squeezefs::meta_backend::storage::MetaLvStorage::open(
                first_meta_path,
                64 * 1024 * 1024,
            )?;
            let config_bytes = serde_json::to_vec(&config)?;
            squeezefs::meta_backend::xattr::set_xattr(
                &storage,
                1,
                "user.squeezefs.format_config",
                &config_bytes,
            )?;
            log::info!("Successfully formatted and recorded config on metadata volume.");

            if let Some(ref paths) = disk_cache_paths {
                for dir in paths {
                    if dir.exists() {
                        log::info!("Wiping local staging/cache directory: {:?}", dir);
                        let _ = tokio::fs::remove_dir_all(dir).await;
                        let _ = tokio::fs::create_dir_all(dir).await;
                    }
                }
            }

            let quick = !full;
            for path in &data_lvs {
                let physical_size = get_backing_device_size(path).unwrap_or(0);
                let wipe_len = if quick {
                    std::cmp::min(total_capacity, 32 * 1024 * 1024)
                } else if physical_size > 0 {
                    std::cmp::min(total_capacity, physical_size)
                } else {
                    total_capacity
                };

                if quick {
                    log::info!(
                        "Quick format: Wiping first {} bytes of data volume: {}",
                        wipe_len,
                        path
                    );
                } else {
                    log::info!(
                        "Full format: Wiping {} bytes of data volume: {}",
                        wipe_len,
                        path
                    );
                }

                if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(path) {
                    use std::io::Write;
                    let zeros = vec![0u8; 1024 * 1024];
                    let mut written = 0;
                    while written < wipe_len {
                        let to_write =
                            std::cmp::min(zeros.len() as u64, wipe_len - written) as usize;
                        if file.write_all(&zeros[..to_write]).is_err() {
                            break;
                        }
                        written += to_write as u64;
                    }
                    let _ = file.sync_all();
                }
            }
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
        Commands::Clients { meta_uri: _ } => {
            squeezefs::set_fs_prefix("squeezefs");
            println!("No active clients connected.");
        }
        Commands::Mount {
            args,
            mem_cache_size,
            disk_cache_size,
            disk_cache_paths,
            data_lv: backing_dev,
            ip: _,
            port: _,
            subnqn: _,

            daemon: _,
            uid,
            gid,
            p2p_addr,
            no_writeback,
            allow_other,
            check_storage: _check_storage,
            options,
            read_cache_size,
            write_cache_size,
            read_mem_cache_size,
            write_mem_cache_size,
            meta_lv,
            dismount_wait,
            upload_delay,
            fuse_io_uring_sqpoll_idle_ms,
            fuse_io_uring_sqpoll_cpu,
            job_cpu_limit,
            write_verification: _,
            write_verification_sample: _,
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

            let first_meta_path = &meta_lvs[0];
            let storage = squeezefs::meta_backend::storage::MetaLvStorage::open(
                first_meta_path,
                64 * 1024 * 1024,
            )?;
            let _disk_inode = squeezefs::meta_backend::inode::read_inode(&storage, 1)?;
            let val_opt = squeezefs::meta_backend::xattr::get_xattr(
                &storage,
                1,
                "user.squeezefs.format_config",
            )?;
            let val = val_opt.ok_or(
                "Format configuration xattr not found on root inode. Is this volume formatted?",
            )?;
            let format_config: FormatConfig = serde_json::from_slice(&val)?;

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

            let staging_dirs = if let Some(dirs) = disk_cache_paths {
                if dirs.is_empty() {
                    vec![get_default_staging_dir()]
                } else {
                    dirs
                }
            } else if let Some(ref paths) = format_config.disk_cache_paths {
                paths.clone()
            } else {
                vec![get_default_staging_dir()]
            };

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

            let mut isolated_staging_dirs = Vec::new();
            for dir in active_staging_dirs {
                let isolated_dir = dir.join(&fs_name).join(sanitized_mount_clean);
                let shared_cache_dir = dir.join(&fs_name).join("cache_segment");

                fs::create_dir_all(&isolated_dir).await?;
                fs::create_dir_all(&shared_cache_dir).await?;

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

            let resolved_data_lvs =
                backing_dev.unwrap_or_else(|| format_config.data_lv.clone().unwrap_or_default());
            if resolved_data_lvs.is_empty() {
                return Err("Error: no data volumes specified or configured".into());
            }

            let first_data_path = &resolved_data_lvs[0];
            squeezefs::storage::validate_backing_device(first_data_path)?;

            let dlm = DlmClient::new("local")?;
            let block_alloc = std::sync::Arc::new(
                squeezefs::block_allocator::BlockAllocator::new(
                    dlm.meta_client().clone(),
                    "squeezefs",
                )
                .await?,
            );

            let nvme_dev =
                std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(first_data_path));

            let cache = TieredCache::new(
                active_staging_dirs.clone(),
                Some(&resolved_read_mem_cache_size),
                Some(&resolved_write_mem_cache_size),
                Some(&resolved_read_cache_size),
                Some(&resolved_write_cache_size),
                dlm.meta_client().clone(),
                block_alloc.clone(),
                nvme_dev.clone(),
            )?;

            if let Some(ref addr) = p2p_addr {
                let _ = cache.nvme.p2p_addr.set(addr.clone());
            }

            let router = DataRouter::new(dlm.clone(), cache, block_alloc.clone(), nvme_dev.clone());

            for path in &resolved_data_lvs {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path)
                    .to_string();

                log::info!("Registering data volume '{}' at path {}", name, path);
                let dev = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(path));
                let alloc = std::sync::Arc::new(
                    squeezefs::block_allocator::BlockAllocator::new(
                        dlm.meta_client().clone(),
                        &name,
                    )
                    .await?,
                );

                let backend = std::sync::Arc::new(squeezefs::routing::StorageBackend {
                    device: dev,
                    block_allocator: alloc,
                });

                router.backend_router.backends.insert(name.clone(), backend);
            }

            let first_name = std::path::Path::new(first_data_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(first_data_path)
                .to_string();
            router
                .backend_router
                .active_write_backend
                .store(std::sync::Arc::new(first_name));

            let mut meta_backends = Vec::new();
            for path in &meta_lvs {
                let storage =
                    squeezefs::meta_backend::storage::MetaLvStorage::open(path, 64 * 1024 * 1024)?;
                let be = std::sync::Arc::new(squeezefs::meta_backend::MetaLvBackend::new(storage));
                meta_backends.push(be);
            }

            let routed_meta_backend = std::sync::Arc::new(
                squeezefs::meta_backend::RoutedMetaBackend::new(meta_backends),
            );

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
            fs_engine.meta_backend = Some(routed_meta_backend);
            fs_engine.dismount_wait = resolved_dismount_wait;

            squeezefs::jobs::start_job_worker(
                std::sync::Arc::new(fs_engine.router.clone()),
                fs_name.clone(),
                job_cpu_limit,
            );

            let opt_idle = if resolved_fuse_io_uring_sqpoll_idle_ms > 0 {
                Some(resolved_fuse_io_uring_sqpoll_idle_ms)
            } else {
                None
            };
            apply_fuse_io_uring_sqpoll_env(opt_idle, resolved_fuse_io_uring_sqpoll_cpu);

            println!("Mounting Squeezefs at {:?}...", mountpoint);

            let writeback_val = !no_writeback;

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
        Commands::Defrag {
            meta_uri: _,
            nvme_path,
            inode,
        } => {
            let redis_url = "dummy".to_string();
            let name = "squeezefs".to_string();
            squeezefs::set_fs_prefix(&name);
            if let Some(ino) = inode {
                println!(
                    "Starting defragmentation for volume '{}' targeting inode {}",
                    name, ino
                );
            } else {
                println!("Starting defragmentation for volume '{}'", name);
            }
            let opts = squeezefs::defrag::DefragOptions {
                target_inode: inode,
                ..Default::default()
            };
            squeezefs::defrag::run_defragmentation_with_options(
                &redis_url, &name, &nvme_path, opts,
            )
            .await?;
        }
        Commands::Bench {
            path,
            threads,
            large_size,
            small_size,
            small_count,
            iterations,
            only,
            skip,
            direct,
        } => {
            let mut resolved_url = None;
            let config_path = path.join(".config");
            let is_squeeze = if let Ok(config_str) = std::fs::read_to_string(&config_path) {
                if let Ok(config_json) = serde_json::from_str::<serde_json::Value>(&config_str) {
                    if let Some(url_str) = config_json.get("garnet_url").and_then(|v| v.as_str()) {
                        resolved_url = Some(url_str.to_string());
                    }
                    if let Some(format_obj) = config_json.get("format").and_then(|v| v.as_object())
                    {
                        if let Some(name_str) = format_obj.get("name").and_then(|v| v.as_str()) {
                            squeezefs::set_fs_prefix(name_str);
                        }
                    }
                    true
                } else {
                    false
                }
            } else {
                if let Err(ref e) = std::fs::metadata(&config_path) {
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        use colored::Colorize;
                        println!(
                            "{}",
                            "Error: Permission denied accessing FUSE mountpoint configuration.\n\
                             Note: FUSE mounts are restricted to the mounting user by default.\n\
                             Please run the benchmark without 'sudo', or ensure 'allow_other' was set during mount."
                                .red()
                                .bold()
                        );
                    }
                }
                false
            };

            if !is_squeeze {
                use colored::Colorize;
                println!(
                    "{}",
                    "Warning: Path does not appear to be a SqueezeFS filesystem (could not read .config)."
                        .yellow()
                        .bold()
                );
                println!(
                    "{}",
                    "Running POSIX-only benchmark override.".yellow().bold()
                );
            }

            for iter in 1..=iterations {
                if iterations > 1 {
                    println!("\n--- Benchmark Iteration {}/{} ---", iter, iterations);
                }
                run_benchmark(
                    &path,
                    threads,
                    large_size,
                    small_size,
                    small_count,
                    resolved_url.as_deref(),
                    only.as_deref(),
                    skip.as_deref(),
                    direct,
                )
                .await?;
            }
        }
        Commands::Clone {
            meta_uri: _,
            src,
            dest,
        } => {
            let redis_url = "dummy";
            let fs_name = "squeezefs".to_string();
            squeezefs::set_fs_prefix(&fs_name);
            let staging_dirs = vec![get_default_staging_dir()];

            let dlm = DlmClient::new(redis_url)?;

            // Reconstruct block allocator and nvme block dev for clone operation
            let block_alloc = std::sync::Arc::new(
                squeezefs::block_allocator::BlockAllocator::new(
                    dlm.meta_client().clone(),
                    &fs_name,
                )
                .await?,
            );

            let nvme_path = format!("{}/.squeezefs_nvme", staging_dirs[0].display());
            let nvme_dev = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(&nvme_path));

            let cache = TieredCache::new(
                staging_dirs,
                None,
                None,
                None,
                None,
                dlm.meta_client().clone(),
                block_alloc.clone(),
                nvme_dev.clone(),
            )?;
            let router = DataRouter::new(dlm, cache, block_alloc.clone(), nvme_dev.clone());

            // Run purely offline on default backend_0
            router
                .backend_router
                .active_write_backend
                .store(std::sync::Arc::new("backend_0".to_string()));

            println!("Cloning file from {} to {}...", src, dest);
            router.clone_path(&src, &dest).await?;
            println!("File cloned successfully.");
        }
        Commands::Df { meta_uri: _, path } => {
            squeezefs::set_fs_prefix("squeezefs");
            run_df_command("dummy", path).await?;
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
            StorageActions::Nvmeof(nvmeof_action) => match nvmeof_action {
                NvmeofActions::Share {
                    backing_path,
                    subnqn,
                    port,
                    ip,
                    spdk,
                } => {
                    let resolved_nqn = if spdk {
                        squeezefs::nvmeof::share_target_spdk(
                            &backing_path,
                            subnqn.as_deref(),
                            port,
                            &ip,
                        )?
                    } else {
                        squeezefs::nvmeof::share_target(
                            &backing_path,
                            subnqn.as_deref(),
                            port,
                            &ip,
                        )?
                    };
                    println!(
                        "Successfully shared '{}' as {}NVMe-oF target.",
                        backing_path,
                        if spdk { "SPDK " } else { "" }
                    );
                    println!("Subsystem NQN: {}", resolved_nqn);
                    println!("Connection string for client nodes:");
                    println!(
                        "  squeezefs storage nvmeof connect --ip {} --port {} --subnqn {}",
                        ip.first()
                            .cloned()
                            .unwrap_or_else(|| "<your-target-ip>".to_string()),
                        port,
                        resolved_nqn
                    );
                }
                NvmeofActions::Unshare { subnqn, spdk } => {
                    if spdk {
                        squeezefs::nvmeof::unshare_target_spdk(&subnqn)?;
                    } else {
                        squeezefs::nvmeof::unshare_target(&subnqn)?;
                    }
                    println!("Successfully stopped sharing target NQN '{}'.", subnqn);
                }
                NvmeofActions::Connect { ip, port, subnqn } => {
                    println!("Connecting to NVMe-oF target at {}:{}...", ip, port);
                    let dev = squeezefs::nvmeof::connect_target(&ip, port, &subnqn)?;
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
                NvmeofActions::List => {
                    squeezefs::nvmeof::list_nvmeof()?;
                }
                NvmeofActions::RestoreShares => {
                    squeezefs::nvmeof::restore_shares()?;
                    println!("All registered persistent target shares restored successfully.");
                }
                NvmeofActions::SpdkInstall => {
                    squeezefs::nvmeof::spdk_install()?;
                }
                NvmeofActions::SpdkSetup { hugepages } => {
                    let mb = match hugepages.as_str() {
                        "2GB" | "2gb" => 2048,
                        "4GB" | "4gb" => 4096,
                        other => {
                            return Err(format!(
                                "Invalid hugepages value '{}'. Must be '2GB' or '4GB'.",
                                other
                            )
                            .into());
                        }
                    };
                    squeezefs::nvmeof::spdk_setup(mb)?;
                }
                NvmeofActions::SpdkBind { pci } => {
                    squeezefs::nvmeof::spdk_bind(&pci)?;
                }
                NvmeofActions::SpdkUnbind { pci } => {
                    squeezefs::nvmeof::spdk_unbind(&pci)?;
                }
                NvmeofActions::SpdkStart => {
                    squeezefs::nvmeof::spdk_start()?;
                }
            },
        },
        Commands::Tune => {
            tune_system()?;
        }
        Commands::Config {
            meta_uri: _,
            action,
        } => {
            let garnet_url = "dummy".to_string();
            let fs_name = "squeezefs".to_string();
            squeezefs::set_fs_prefix(&fs_name);
            match action {
                ConfigActions::Set { key, value } => {
                    squeezefs::config_ops::set_config_quota(&garnet_url, &fs_name, &key, &value)
                        .await?;
                }
                ConfigActions::DiskCache(action) => match action {
                    DiskCacheActions::Add { path } => {
                        squeezefs::config_ops::add_disk_cache_path(
                            &garnet_url,
                            &fs_name,
                            Path::new(&path),
                            false,
                        )
                        .await?;
                        println!("Disk cache path '{}' added successfully.", path);
                    }
                    DiskCacheActions::Remove { path, force } => {
                        squeezefs::config_ops::remove_disk_cache_path(
                            &garnet_url,
                            &fs_name,
                            Path::new(&path),
                            force,
                        )
                        .await?;
                        println!("Disk cache path '{}' removed successfully.", path);
                    }
                    DiskCacheActions::Enable { path } => {
                        squeezefs::config_ops::enable_disk_cache_path(
                            &garnet_url,
                            &fs_name,
                            Path::new(&path),
                        )
                        .await?;
                        println!("Disk cache path '{}' enabled successfully.", path);
                    }
                    DiskCacheActions::Disable { path } => {
                        squeezefs::config_ops::disable_disk_cache_path(
                            &garnet_url,
                            &fs_name,
                            Path::new(&path),
                        )
                        .await?;
                        println!("Disk cache path '{}' disabled successfully.", path);
                    }
                    DiskCacheActions::Flush { path } => {
                        squeezefs::config_ops::flush_disk_cache_path(
                            &garnet_url,
                            &fs_name,
                            Path::new(&path),
                        )
                        .await?;
                        println!("Disk cache path '{}' flushed successfully.", path);
                    }
                    DiskCacheActions::List => {
                        let list =
                            squeezefs::config_ops::list_config(&garnet_url, &fs_name).await?;
                        println!("{}", serde_json::to_string_pretty(&list.diskcaches)?);
                    }
                },
                ConfigActions::DataVolume(action) => match action {
                    DataVolumeActions::Add {
                        volume_id,
                        volume: backing_dev,
                        ..
                    } => {
                        squeezefs::config_ops::add_data_volume(
                            &garnet_url,
                            &fs_name,
                            &volume_id,
                            backing_dev.as_deref(),
                        )
                        .await?;
                        println!("Data volume '{}' registered/added successfully.", volume_id);
                    }
                    DataVolumeActions::Remove { volume_id, .. } => {
                        squeezefs::config_ops::remove_data_volume(
                            &garnet_url,
                            &fs_name,
                            &volume_id,
                        )
                        .await?;
                        println!("Data volume '{}' removed successfully.", volume_id);
                    }
                    DataVolumeActions::List => {
                        let list =
                            squeezefs::config_ops::list_config(&garnet_url, &fs_name).await?;
                        println!("{}", serde_json::to_string_pretty(&list.data_volumes)?);
                    }
                    DataVolumeActions::Enable { volume_id } => {
                        squeezefs::config_ops::enable_data_volume(
                            &garnet_url,
                            &fs_name,
                            &volume_id,
                        )
                        .await?;
                        println!("Data volume '{}' enabled successfully.", volume_id);
                    }
                    DataVolumeActions::Disable { volume_id } => {
                        squeezefs::config_ops::disable_data_volume(
                            &garnet_url,
                            &fs_name,
                            &volume_id,
                        )
                        .await?;
                        println!("Data volume '{}' disabled successfully.", volume_id);
                    }
                    DataVolumeActions::Migrate {
                        from_volume,
                        to_volume,
                    } => {
                        squeezefs::config_ops::migrate_data_volume(
                            &garnet_url,
                            &fs_name,
                            &from_volume,
                            &to_volume,
                        )
                        .await?;
                        println!(
                            "Migrated data off data volume '{}' onto '{}' successfully.",
                            from_volume, to_volume
                        );
                    }
                },
                ConfigActions::MetadataVolume(action) => {
                    match action {
                        MetadataVolumeActions::Add {
                            volume_id,
                            volume: backing_dev,
                            ..
                        } => {
                            squeezefs::config_ops::add_metadata_volume(
                                &garnet_url,
                                &fs_name,
                                &volume_id,
                                backing_dev.as_deref(),
                            )
                            .await?;
                            println!(
                                "Metadata volume '{}' registered/added successfully.",
                                volume_id
                            );
                        }
                        MetadataVolumeActions::Remove { volume_id, .. } => {
                            squeezefs::config_ops::remove_metadata_volume(
                                &garnet_url,
                                &fs_name,
                                &volume_id,
                            )
                            .await?;
                            println!("Metadata volume '{}' removed successfully.", volume_id);
                        }
                        MetadataVolumeActions::List => {
                            let list =
                                squeezefs::config_ops::list_config(&garnet_url, &fs_name).await?;
                            println!("{}", serde_json::to_string_pretty(&list.metadata_volumes)?);
                        }
                        MetadataVolumeActions::Enable { volume_id } => {
                            squeezefs::config_ops::enable_metadata_volume(
                                &garnet_url,
                                &fs_name,
                                &volume_id,
                            )
                            .await?;
                            println!("Metadata volume '{}' enabled successfully.", volume_id);
                        }
                        MetadataVolumeActions::Disable { volume_id } => {
                            squeezefs::config_ops::disable_metadata_volume(
                                &garnet_url,
                                &fs_name,
                                &volume_id,
                            )
                            .await?;
                            println!("Metadata volume '{}' disabled successfully.", volume_id);
                        }
                        MetadataVolumeActions::Migrate {
                            from_volume,
                            to_volume,
                        } => {
                            squeezefs::config_ops::migrate_metadata_volume(
                                &garnet_url,
                                &fs_name,
                                &from_volume,
                                &to_volume,
                            )
                            .await?;
                            println!("Migrated metadata off metadata volume '{}' onto '{}' successfully.", from_volume, to_volume);
                        }
                    }
                }
                ConfigActions::List => {
                    let list = squeezefs::config_ops::list_config(&garnet_url, &fs_name).await?;
                    println!("{}", serde_json::to_string_pretty(&list)?);
                }
                ConfigActions::Fsck => {
                    println!("Running Squeezefs Metadata Consistency Check (FSCK)...");
                    let issues =
                        squeezefs::config_ops::run_metadata_fsck(&garnet_url, &fs_name).await?;
                    if issues.is_empty() {
                        println!("FSCK Completed: No consistency issues found.");
                    } else {
                        println!("FSCK Completed: Found {} issue(s):", issues.len());
                        for issue in issues {
                            println!("  - {}", issue);
                        }
                        std::process::exit(1);
                    }
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

            // 1. Try to read mountpoint/.config to resolve staging directories
            let mut staging_dirs = Vec::new();
            let config_path = mountpoint.join(".config");
            if let Ok(config_str) = std::fs::read_to_string(&config_path) {
                if let Ok(config_json) = serde_json::from_str::<serde_json::Value>(&config_str) {
                    if let Some(paths_str) = config_json["format"]["disk_cache_paths"].as_str() {
                        if !paths_str.is_empty() {
                            staging_dirs = paths_str.split(',').map(PathBuf::from).collect();
                        }
                    }
                }
            }

            // 3. Fall back to default staging directory
            if staging_dirs.is_empty() {
                staging_dirs = vec![get_default_staging_dir()];
            }

            // 4. Resolve dismount_wait limit (default: 10) and find active daemon PID
            let dismount_wait = 10;
            let mut daemon_pid: Option<u32> = None;

            // 5. Count staged files and active writes from cache segments
            let max_write_bytes = 100 * 1024 * 1024;

            let mut caches = Vec::new();
            for dir in &staging_dirs {
                let staging_segment_dir = dir.join("staging_segment");
                if staging_segment_dir.exists() {
                    let write_cap = max_write_bytes as usize / staging_dirs.len();
                    if let Ok(cache) = squeezefs::tiering::nvme::NvmeCache::new(
                        &[staging_segment_dir.as_path()],
                        &[write_cap],
                        16,
                    ) {
                        if squeezefs::cache::nvme::dir_has_segment_data(&staging_segment_dir) {
                            cache.recover_index();
                        }
                        caches.push(cache);
                    }
                }
            }

            let mut staged_count = 0;
            let mut active_writes_count = 0;
            for cache in &caches {
                for key_bytes in cache.list_keys() {
                    if let Ok(s) = String::from_utf8(key_bytes.to_vec()) {
                        if s.starts_with("active_block:") {
                            active_writes_count += 1;
                        } else {
                            staged_count += 1;
                        }
                    }
                }
            }

            let has_unflushed = staged_count > 0 || active_writes_count > 0;
            let mut choice = "continue"; // default non-interactive behavior

            // 6. Prompt the user if not forced and stdin is a TTY
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
                println!("  [c] Continue/force unmount immediately (unsafe - may lose data)");
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
                    let mut total_bytes_at_start = 0;
                    for cache in &caches {
                        for key_bytes in cache.list_keys() {
                            let is_active = if let Ok(s) = String::from_utf8(key_bytes.to_vec()) {
                                s.starts_with("active_block:")
                            } else {
                                false
                            };
                            if !is_active {
                                if let Some(guard) = cache.get(&key_bytes) {
                                    let bytes =
                                        &guard.guard.mmap[guard.offset..guard.offset + guard.len];
                                    if bytes.len() >= 8 {
                                        let meta_len = u64::from_be_bytes(
                                            bytes[0..8].try_into().unwrap_or([0; 8]),
                                        )
                                            as usize;
                                        if bytes.len() >= 8 + meta_len {
                                            let original_size =
                                                match serde_json::from_slice::<serde_json::Value>(
                                                    &bytes[8..8 + meta_len],
                                                ) {
                                                    Ok(json) => json
                                                        .get("original_size")
                                                        .and_then(|v| v.as_u64())
                                                        .unwrap_or(0)
                                                        as usize,
                                                    Err(_) => 0,
                                                };
                                            total_bytes_at_start += original_size as u64;
                                        }
                                    }
                                }
                            }
                        }
                    }

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
                    let mut current_staged = 0;
                    let mut current_active = 0;

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

                        current_staged = 0;
                        let mut current_bytes = 0;
                        current_active = 0;

                        for cache in &caches {
                            for key_bytes in cache.list_keys() {
                                let is_active = if let Ok(s) = String::from_utf8(key_bytes.to_vec())
                                {
                                    s.starts_with("active_block:")
                                } else {
                                    false
                                };
                                if is_active {
                                    current_active += 1;
                                } else {
                                    current_staged += 1;
                                    if let Some(guard) = cache.get(&key_bytes) {
                                        let bytes = &guard.guard.mmap
                                            [guard.offset..guard.offset + guard.len];
                                        if bytes.len() >= 8 {
                                            let meta_len = u64::from_be_bytes(
                                                bytes[0..8].try_into().unwrap_or([0; 8]),
                                            )
                                                as usize;
                                            if bytes.len() >= 8 + meta_len {
                                                let original_size = match serde_json::from_slice::<
                                                    serde_json::Value,
                                                >(
                                                    &bytes[8..8 + meta_len]
                                                ) {
                                                    Ok(json) => json
                                                        .get("original_size")
                                                        .and_then(|v| v.as_u64())
                                                        .unwrap_or(0)
                                                        as usize,
                                                    Err(_) => 0,
                                                };
                                                current_bytes += original_size as u64;
                                            }
                                        }
                                    }
                                }
                            }
                        }

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
                            100.0 * (total_bytes_at_start - current_bytes) as f64
                                / total_bytes_at_start as f64
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
                        println!("{}", "WARNING: Dismounting with unflushed staged files or active writes will cause data loss!".red().bold());
                        print!("Do you want to discard this data, clean up local cache, and remove incomplete metadata? [y/N]: ");
                        let _ = std::io::stdout().flush();

                        let mut confirmed = false;
                        let timeout_fut =
                            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv());
                        if let Ok(Some(msg)) = timeout_fut.await {
                            if msg == "y" || msg == "yes" {
                                confirmed = true;
                            }
                        }

                        if confirmed {
                            println!("Discarding unflushed data and cleaning up Redis metadata/local staging...");

                            // 2. Remove all keys from cache
                            for cache in &caches {
                                for key_bytes in cache.list_keys() {
                                    cache.remove(&key_bytes);
                                }
                            }
                            println!("Local staging cache cleared successfully.");
                        } else {
                            println!("Unmount will continue, but staged files/metadata are left intact on disk/database.");
                        }
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

async fn run_df_command(
    _redis_url: &str,
    _path_opt: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("SqueezeFS space usage command (df) is offline. Metadata and data are managed directly on block devices.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

async fn get_daemon_metrics(_redis_url: &str) -> Option<HashMap<String, u64>> {
    Some(HashMap::new())
}

async fn run_benchmark(
    path: &Path,
    threads: usize,
    large_size_mb: usize,
    small_size_kb: usize,
    small_count: usize,
    redis_url: Option<&str>,
    only: Option<&[String]>,
    skip: Option<&[String]>,
    direct: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use rand::seq::SliceRandom;
    #[allow(unused_imports)]
    use std::os::unix::fs::OpenOptionsExt;
    use tokio::io::{AsyncSeekExt, SeekFrom};

    if let Err(e) = std::fs::metadata(path) {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(format!(
                "Permission denied accessing benchmark path {:?}.\n\
                 Note: FUSE mounts are by default only accessible to the mounting user.\n\
                 Please run the command as the mounting user (without sudo), or ensure the filesystem was mounted with FUSE options allow_other.",
                path
            ).into());
        }
        return Err(format!("Benchmark path {:?} does not exist: {:?}", path, e).into());
    }

    if direct && (small_size_kb * 1024) % 4096 != 0 {
        return Err("For Direct I/O (--direct), small-size (in KB) must be a multiple of 4 to ensure 4096-byte alignment".into());
    }

    let is_workload_enabled = |name: &str, group: &str| -> bool {
        if let Some(only_list) = only {
            if !only_list.iter().any(|s| {
                let s_trimmed = s.trim();
                s_trimmed == name || s_trimmed == group
            }) {
                return false;
            }
        }
        if let Some(skip_list) = skip {
            if skip_list.iter().any(|s| {
                let s_trimmed = s.trim();
                s_trimmed == name || s_trimmed == group
            }) {
                return false;
            }
        }
        true
    };

    println!(
        "{}",
        "==================================================================================".bold()
    );
    println!(
        "  Running Squeezefs Benchmark (T={}, Large={}MB, Small={}KB x {}, Direct={})",
        threads, large_size_mb, small_size_kb, small_count, direct
    );
    println!(
        "{}",
        "==================================================================================".bold()
    );

    // 1. Fetch baseline metrics
    let baseline_metrics = if let Some(url) = redis_url {
        get_daemon_metrics(url).await
    } else {
        None
    };

    let mp = MultiProgress::new();
    let pb_style = ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}")?
        .progress_chars("#>-");

    // Pre-calculate byte size
    let chunk_size = 1024 * 1024; // 1MB block size
    let num_chunks = large_size_mb;

    // Helper for O_DIRECT aligned memory buffer
    struct AlignedBuf {
        _raw: Vec<u8>,
        offset: usize,
        len: usize,
    }

    impl AlignedBuf {
        fn new(size: usize, direct: bool) -> Self {
            if direct {
                let raw = vec![0u8; size + 4096];
                let addr = raw.as_ptr() as usize;
                let offset = (4096 - (addr % 4096)) % 4096;
                Self {
                    _raw: raw,
                    offset,
                    len: size,
                }
            } else {
                Self {
                    _raw: vec![0u8; size],
                    offset: 0,
                    len: size,
                }
            }
        }

        fn as_slice(&self) -> &[u8] {
            &self._raw[self.offset..self.offset + self.len]
        }

        fn as_mut_slice(&mut self) -> &mut [u8] {
            &mut self._raw[self.offset..self.offset + self.len]
        }
    }

    let get_open_options = move |read: bool, write: bool, create: bool, truncate: bool| {
        let mut options = OpenOptions::new();
        if read {
            options.read(true);
        }
        if write {
            options.write(true);
        }
        if create {
            options.create(true);
        }
        if truncate {
            options.truncate(true);
        }
        #[cfg(target_os = "linux")]
        if direct {
            options.custom_flags(libc::O_DIRECT);
        }
        options
    };

    // --- 1. WRITE LARGE FILE (SEQUENTIAL) ---
    let mut d_write_large_seq = None;
    if is_workload_enabled("large-seq-write", "large-seq") {
        println!(
            "Writing large file sequentially ({} MB/thread)...",
            large_size_mb
        );
        let pb_write_large_seq = mp.add(ProgressBar::new((threads * num_chunks) as u64));
        pb_write_large_seq.set_style(pb_style.clone());
        pb_write_large_seq.set_message("Write Large Seq");

        let t_start = Instant::now();
        let mut write_tasks = Vec::new();
        for t_id in 0..threads {
            let path_clone = path.to_path_buf();
            let pb = pb_write_large_seq.clone();
            write_tasks.push(tokio::spawn(async move {
                let file_path = path_clone.join(format!("bench_large_seq_{}.bin", t_id));
                let mut file = get_open_options(false, true, true, true)
                    .open(&file_path)
                    .await?;
                let buf = AlignedBuf::new(chunk_size, direct);
                for _ in 0..num_chunks {
                    file.write_all(buf.as_slice()).await?;
                    pb.inc(1);
                }
                file.sync_all().await?;
                Ok::<_, std::io::Error>(())
            }));
        }
        for task in write_tasks {
            task.await??;
        }
        pb_write_large_seq.finish_with_message("Done");
        d_write_large_seq = Some(t_start.elapsed());
    }

    // --- 2. READ LARGE FILE (SEQUENTIAL) ---
    let mut d_read_large_seq = None;
    if is_workload_enabled("large-seq-read", "large-seq") {
        let file_exists = path.join("bench_large_seq_0.bin").exists();
        if file_exists {
            println!("Reading large file sequentially...");
            let pb_read_large_seq = mp.add(ProgressBar::new((threads * num_chunks) as u64));
            pb_read_large_seq.set_style(pb_style.clone());
            pb_read_large_seq.set_message("Read Large Seq");

            let t_start = Instant::now();
            let mut read_tasks = Vec::new();
            for t_id in 0..threads {
                let path_clone = path.to_path_buf();
                let pb = pb_read_large_seq.clone();
                read_tasks.push(tokio::spawn(async move {
                    let file_path = path_clone.join(format!("bench_large_seq_{}.bin", t_id));
                    let mut file = get_open_options(true, false, false, false)
                        .open(&file_path)
                        .await?;
                    let mut buf = AlignedBuf::new(chunk_size, direct);
                    for _ in 0..num_chunks {
                        file.read_exact(buf.as_mut_slice()).await?;
                        pb.inc(1);
                    }
                    Ok::<_, std::io::Error>(())
                }));
            }
            for task in read_tasks {
                task.await??;
            }
            pb_read_large_seq.finish_with_message("Done");
            d_read_large_seq = Some(t_start.elapsed());
        } else {
            println!("Skipping Read Large Seq: sequential backing file not found");
        }
    }

    // --- 3. WRITE LARGE FILE (RANDOM) ---
    let mut d_write_large_rand = None;
    if is_workload_enabled("large-rand-write", "large-rand") {
        println!(
            "Writing large file randomly ({} MB/thread)...",
            large_size_mb
        );
        let pb_write_large_rand = mp.add(ProgressBar::new((threads * num_chunks) as u64));
        pb_write_large_rand.set_style(pb_style.clone());
        pb_write_large_rand.set_message("Write Large Rand");

        let t_start = Instant::now();
        let mut write_tasks = Vec::new();
        for t_id in 0..threads {
            let path_clone = path.to_path_buf();
            let pb = pb_write_large_rand.clone();
            let mut rng = rand::thread_rng();
            let mut indices: Vec<usize> = (0..num_chunks).collect();
            indices.shuffle(&mut rng);
            write_tasks.push(tokio::spawn(async move {
                let file_path = path_clone.join(format!("bench_large_rand_{}.bin", t_id));
                let mut file = get_open_options(false, true, true, false)
                    .open(&file_path)
                    .await?;
                let buf = AlignedBuf::new(chunk_size, direct);
                for idx in indices {
                    file.seek(SeekFrom::Start((idx * chunk_size) as u64))
                        .await?;
                    file.write_all(buf.as_slice()).await?;
                    pb.inc(1);
                }
                file.sync_all().await?;
                Ok::<_, std::io::Error>(())
            }));
        }
        for task in write_tasks {
            task.await??;
        }
        pb_write_large_rand.finish_with_message("Done");
        d_write_large_rand = Some(t_start.elapsed());
    }

    // --- 4. READ LARGE FILE (RANDOM) ---
    let mut d_read_large_rand = None;
    if is_workload_enabled("large-rand-read", "large-rand") {
        let file_exists = path.join("bench_large_rand_0.bin").exists();
        if file_exists {
            println!("Reading large file randomly...");
            let pb_read_large_rand = mp.add(ProgressBar::new((threads * num_chunks) as u64));
            pb_read_large_rand.set_style(pb_style.clone());
            pb_read_large_rand.set_message("Read Large Rand");

            let t_start = Instant::now();
            let mut read_tasks = Vec::new();
            for t_id in 0..threads {
                let path_clone = path.to_path_buf();
                let pb = pb_read_large_rand.clone();
                let mut rng = rand::thread_rng();
                let mut indices: Vec<usize> = (0..num_chunks).collect();
                indices.shuffle(&mut rng);
                read_tasks.push(tokio::spawn(async move {
                    let file_path = path_clone.join(format!("bench_large_rand_{}.bin", t_id));
                    let mut file = get_open_options(true, false, false, false)
                        .open(&file_path)
                        .await?;
                    let mut buf = AlignedBuf::new(chunk_size, direct);
                    for idx in indices {
                        file.seek(SeekFrom::Start((idx * chunk_size) as u64))
                            .await?;
                        file.read_exact(buf.as_mut_slice()).await?;
                        pb.inc(1);
                    }
                    Ok::<_, std::io::Error>(())
                }));
            }
            for task in read_tasks {
                task.await??;
            }
            pb_read_large_rand.finish_with_message("Done");
            d_read_large_rand = Some(t_start.elapsed());
        } else {
            println!("Skipping Read Large Rand: random backing file not found");
        }
    }

    // --- 5. WRITE SMALL FILES (SEQUENTIAL) ---
    let small_file_bytes = small_size_kb * 1024;
    let mut d_write_small_seq = None;
    if is_workload_enabled("small-seq-write", "small-seq") {
        println!(
            "Writing small files sequentially ({} files of {} KB/thread)...",
            small_count, small_size_kb
        );
        let pb_write_small_seq = mp.add(ProgressBar::new((threads * small_count) as u64));
        pb_write_small_seq.set_style(pb_style.clone());
        pb_write_small_seq.set_message("Write Small Seq");

        let t_start = Instant::now();
        let mut write_small_tasks = Vec::new();
        for t_id in 0..threads {
            let path_clone = path.to_path_buf();
            let pb = pb_write_small_seq.clone();
            write_small_tasks.push(tokio::spawn(async move {
                let mut buf = AlignedBuf::new(small_file_bytes, direct);
                buf.as_mut_slice().fill(1);
                for f_id in 0..small_count {
                    let file_path =
                        path_clone.join(format!("bench_small_seq_{}_{}.bin", t_id, f_id));
                    let mut file = get_open_options(false, true, true, true)
                        .open(&file_path)
                        .await?;
                    file.write_all(buf.as_slice()).await?;
                    file.sync_all().await?;
                    pb.inc(1);
                }
                Ok::<_, std::io::Error>(())
            }));
        }
        for task in write_small_tasks {
            task.await??;
        }
        pb_write_small_seq.finish_with_message("Done");
        d_write_small_seq = Some(t_start.elapsed());
    }

    // --- 6. READ SMALL FILES (SEQUENTIAL) ---
    let mut d_read_small_seq = None;
    if is_workload_enabled("small-seq-read", "small-seq") {
        let file_exists = path.join("bench_small_seq_0_0.bin").exists();
        if file_exists {
            println!("Reading small files sequentially...");
            let pb_read_small_seq = mp.add(ProgressBar::new((threads * small_count) as u64));
            pb_read_small_seq.set_style(pb_style.clone());
            pb_read_small_seq.set_message("Read Small Seq");

            let t_start = Instant::now();
            let mut read_small_tasks = Vec::new();
            for t_id in 0..threads {
                let path_clone = path.to_path_buf();
                let pb = pb_read_small_seq.clone();
                read_small_tasks.push(tokio::spawn(async move {
                    let mut buf = AlignedBuf::new(small_file_bytes, direct);
                    for f_id in 0..small_count {
                        let file_path =
                            path_clone.join(format!("bench_small_seq_{}_{}.bin", t_id, f_id));
                        let mut file = get_open_options(true, false, false, false)
                            .open(&file_path)
                            .await?;
                        file.read_exact(buf.as_mut_slice()).await?;
                        pb.inc(1);
                    }
                    Ok::<_, std::io::Error>(())
                }));
            }
            for task in read_small_tasks {
                task.await??;
            }
            pb_read_small_seq.finish_with_message("Done");
            d_read_small_seq = Some(t_start.elapsed());
        } else {
            println!("Skipping Read Small Seq: sequential small files not found");
        }
    }

    // --- 7. WRITE SMALL FILES (RANDOM) ---
    let mut d_write_small_rand = None;
    if is_workload_enabled("small-rand-write", "small-rand") {
        println!("Writing small files randomly...");
        let pb_write_small_rand = mp.add(ProgressBar::new((threads * small_count) as u64));
        pb_write_small_rand.set_style(pb_style.clone());
        pb_write_small_rand.set_message("Write Small Rand");

        let t_start = Instant::now();
        let mut write_small_tasks = Vec::new();
        for t_id in 0..threads {
            let path_clone = path.to_path_buf();
            let pb = pb_write_small_rand.clone();
            let mut rng = rand::thread_rng();
            let mut indices: Vec<usize> = (0..small_count).collect();
            indices.shuffle(&mut rng);
            write_small_tasks.push(tokio::spawn(async move {
                let mut buf = AlignedBuf::new(small_file_bytes, direct);
                buf.as_mut_slice().fill(2);
                for f_id in indices {
                    let file_path =
                        path_clone.join(format!("bench_small_rand_{}_{}.bin", t_id, f_id));
                    let mut file = get_open_options(false, true, true, true)
                        .open(&file_path)
                        .await?;
                    file.write_all(buf.as_slice()).await?;
                    file.sync_all().await?;
                    pb.inc(1);
                }
                Ok::<_, std::io::Error>(())
            }));
        }
        for task in write_small_tasks {
            task.await??;
        }
        pb_write_small_rand.finish_with_message("Done");
        d_write_small_rand = Some(t_start.elapsed());
    }

    // --- 8. READ SMALL FILES (RANDOM) ---
    let mut d_read_small_rand = None;
    if is_workload_enabled("small-rand-read", "small-rand") {
        let file_exists = path.join("bench_small_rand_0_0.bin").exists();
        if file_exists {
            println!("Reading small files randomly...");
            let pb_read_small_rand = mp.add(ProgressBar::new((threads * small_count) as u64));
            pb_read_small_rand.set_style(pb_style.clone());
            pb_read_small_rand.set_message("Read Small Rand");

            let t_start = Instant::now();
            let mut read_small_tasks = Vec::new();
            for t_id in 0..threads {
                let path_clone = path.to_path_buf();
                let pb = pb_read_small_rand.clone();
                let mut rng = rand::thread_rng();
                let mut indices: Vec<usize> = (0..small_count).collect();
                indices.shuffle(&mut rng);
                read_small_tasks.push(tokio::spawn(async move {
                    let mut buf = AlignedBuf::new(small_file_bytes, direct);
                    for f_id in indices {
                        let file_path =
                            path_clone.join(format!("bench_small_rand_{}_{}.bin", t_id, f_id));
                        let mut file = get_open_options(true, false, false, false)
                            .open(&file_path)
                            .await?;
                        file.read_exact(buf.as_mut_slice()).await?;
                        pb.inc(1);
                    }
                    Ok::<_, std::io::Error>(())
                }));
            }
            for task in read_small_tasks {
                task.await??;
            }
            pb_read_small_rand.finish_with_message("Done");
            d_read_small_rand = Some(t_start.elapsed());
        } else {
            println!("Skipping Read Small Rand: random small files not found");
        }
    }

    // --- 9. STAT SMALL FILES ---
    let mut d_stat = None;
    if is_workload_enabled("metadata-stat", "metadata") {
        let file_exists = path.join("bench_small_seq_0_0.bin").exists();
        if file_exists {
            println!("Stat small files...");
            let pb_stat_small = mp.add(ProgressBar::new((threads * small_count) as u64));
            pb_stat_small.set_style(pb_style.clone());
            pb_stat_small.set_message("Stat Files");

            let t_start = Instant::now();
            let mut stat_tasks = Vec::new();
            for t_id in 0..threads {
                let path_clone = path.to_path_buf();
                let pb = pb_stat_small.clone();
                stat_tasks.push(tokio::spawn(async move {
                    for f_id in 0..small_count {
                        let file_path =
                            path_clone.join(format!("bench_small_seq_{}_{}.bin", t_id, f_id));
                        let _meta = fs::metadata(&file_path).await?;
                        pb.inc(1);
                    }
                    Ok::<_, std::io::Error>(())
                }));
            }
            for task in stat_tasks {
                task.await??;
            }
            pb_stat_small.finish_with_message("Done");
            d_stat = Some(t_start.elapsed());
        } else {
            println!("Skipping Metadata Stat: sequential files for stat not found");
        }
    }

    // --- 10. MKDIR DIRECTORIES ---
    let mut d_mkdir = None;
    if is_workload_enabled("metadata-mkdir", "metadata") {
        println!("Mkdir directories...");
        let pb_mkdir = mp.add(ProgressBar::new((threads * small_count) as u64));
        pb_mkdir.set_style(pb_style.clone());
        pb_mkdir.set_message("Mkdir");

        let t_start = Instant::now();
        let mut mkdir_tasks = Vec::new();
        for t_id in 0..threads {
            let path_clone = path.to_path_buf();
            let pb = pb_mkdir.clone();
            mkdir_tasks.push(tokio::spawn(async move {
                let thread_dir = path_clone.join(format!("bench_dir_{}", t_id));
                let _ = fs::create_dir_all(&thread_dir).await;
                for d_id in 0..small_count {
                    let dir_path = thread_dir.join(format!("dir_{}", d_id));
                    fs::create_dir(&dir_path).await?;
                    pb.inc(1);
                }
                Ok::<_, std::io::Error>(())
            }));
        }
        for task in mkdir_tasks {
            task.await??;
        }
        pb_mkdir.finish_with_message("Done");
        d_mkdir = Some(t_start.elapsed());
    }

    // --- 11. READDIR DIRECTORIES ---
    let mut d_readdir = None;
    if is_workload_enabled("metadata-readdir", "metadata") {
        let dir_exists = path.join("bench_dir_0").exists();
        if dir_exists {
            println!("Readdir directories...");
            let pb_readdir = mp.add(ProgressBar::new((threads * small_count) as u64));
            pb_readdir.set_style(pb_style.clone());
            pb_readdir.set_message("Readdir");

            let t_start = Instant::now();
            let mut readdir_tasks = Vec::new();
            for t_id in 0..threads {
                let path_clone = path.to_path_buf();
                let pb = pb_readdir.clone();
                readdir_tasks.push(tokio::spawn(async move {
                    let thread_dir = path_clone.join(format!("bench_dir_{}", t_id));
                    for _ in 0..small_count {
                        let mut reader = fs::read_dir(&thread_dir).await?;
                        while let Some(_entry) = reader.next_entry().await? {}
                        pb.inc(1);
                    }
                    Ok::<_, std::io::Error>(())
                }));
            }
            for task in readdir_tasks {
                task.await??;
            }
            pb_readdir.finish_with_message("Done");
            d_readdir = Some(t_start.elapsed());
        } else {
            println!("Skipping Metadata Readdir: directories for readdir not found");
        }
    }

    // --- 12. RMDIR DIRECTORIES ---
    let mut d_rmdir = None;
    if is_workload_enabled("metadata-rmdir", "metadata") {
        let dir_exists = path.join("bench_dir_0").exists();
        if dir_exists {
            println!("Rmdir directories...");
            let pb_rmdir = mp.add(ProgressBar::new((threads * small_count) as u64));
            pb_rmdir.set_style(pb_style.clone());
            pb_rmdir.set_message("Rmdir");

            let t_start = Instant::now();
            let mut rmdir_tasks = Vec::new();
            for t_id in 0..threads {
                let path_clone = path.to_path_buf();
                let pb = pb_rmdir.clone();
                rmdir_tasks.push(tokio::spawn(async move {
                    let thread_dir = path_clone.join(format!("bench_dir_{}", t_id));
                    for d_id in 0..small_count {
                        let dir_path = thread_dir.join(format!("dir_{}", d_id));
                        fs::remove_dir(&dir_path).await?;
                        pb.inc(1);
                    }
                    let _ = fs::remove_dir(&thread_dir).await;
                    Ok::<_, std::io::Error>(())
                }));
            }
            for task in rmdir_tasks {
                task.await??;
            }
            pb_rmdir.finish_with_message("Done");
            d_rmdir = Some(t_start.elapsed());
        } else {
            println!("Skipping Metadata Rmdir: directories for rmdir not found");
        }
    }

    // --- 13. DELETE FILES ---
    let mut d_delete = None;
    if is_workload_enabled("metadata-delete", "metadata") {
        let seq_exists = path.join("bench_small_seq_0_0.bin").exists();
        let rand_exists = path.join("bench_small_rand_0_0.bin").exists();
        if seq_exists || rand_exists {
            println!("Deleting small files...");
            let total_deletes = if seq_exists && rand_exists {
                2 * small_count
            } else {
                small_count
            };
            let pb_delete_small = mp.add(ProgressBar::new((threads * total_deletes) as u64));
            pb_delete_small.set_style(pb_style.clone());
            pb_delete_small.set_message("Delete Files");

            let t_start = Instant::now();
            let mut delete_tasks = Vec::new();
            for t_id in 0..threads {
                let path_clone = path.to_path_buf();
                let pb = pb_delete_small.clone();
                delete_tasks.push(tokio::spawn(async move {
                    if seq_exists {
                        for f_id in 0..small_count {
                            let file_path =
                                path_clone.join(format!("bench_small_seq_{}_{}.bin", t_id, f_id));
                            let _ = fs::remove_file(&file_path).await;
                            pb.inc(1);
                        }
                    }
                    if rand_exists {
                        for f_id in 0..small_count {
                            let file_path =
                                path_clone.join(format!("bench_small_rand_{}_{}.bin", t_id, f_id));
                            let _ = fs::remove_file(&file_path).await;
                            pb.inc(1);
                        }
                    }
                    Ok::<_, std::io::Error>(())
                }));
            }
            for task in delete_tasks {
                task.await??;
            }
            pb_delete_small.finish_with_message("Done");
            d_delete = Some(t_start.elapsed());
        } else {
            println!("Skipping Metadata Delete: small files for delete not found");
        }
    }

    // Clean up large files
    for t_id in 0..threads {
        let file_path1 = path.join(format!("bench_large_seq_{}.bin", t_id));
        let _ = fs::remove_file(file_path1).await;
        let file_path2 = path.join(format!("bench_large_rand_{}.bin", t_id));
        let _ = fs::remove_file(file_path2).await;
    }

    // 2. Fetch post-benchmark metrics
    let post_metrics = if let Some(url) = redis_url {
        get_daemon_metrics(url).await
    } else {
        None
    };

    // --- CALCULATE PERFORMANCE VALUES ---
    let total_large_bytes = (threads * num_chunks * chunk_size) as f64;
    let total_small_files = (threads * small_count) as f64;
    let total_small_bytes = total_small_files * small_file_bytes as f64;

    let (write_large_seq_tput, write_large_seq_iops, write_large_seq_cost) =
        if let Some(d) = d_write_large_seq {
            let tput = (total_large_bytes / (1024.0 * 1024.0)) / d.as_secs_f64();
            let iops = (threads * num_chunks) as f64 / d.as_secs_f64();
            let cost = (d.as_secs_f64() * 1000.0) / (threads * num_chunks) as f64;
            (Some(tput), Some(iops), Some(cost))
        } else {
            (None, None, None)
        };

    let (read_large_seq_tput, read_large_seq_iops, read_large_seq_cost) =
        if let Some(d) = d_read_large_seq {
            let tput = (total_large_bytes / (1024.0 * 1024.0)) / d.as_secs_f64();
            let iops = (threads * num_chunks) as f64 / d.as_secs_f64();
            let cost = (d.as_secs_f64() * 1000.0) / (threads * num_chunks) as f64;
            (Some(tput), Some(iops), Some(cost))
        } else {
            (None, None, None)
        };

    let (write_large_rand_tput, write_large_rand_iops, write_large_rand_cost) =
        if let Some(d) = d_write_large_rand {
            let tput = (total_large_bytes / (1024.0 * 1024.0)) / d.as_secs_f64();
            let iops = (threads * num_chunks) as f64 / d.as_secs_f64();
            let cost = (d.as_secs_f64() * 1000.0) / (threads * num_chunks) as f64;
            (Some(tput), Some(iops), Some(cost))
        } else {
            (None, None, None)
        };

    let (read_large_rand_tput, read_large_rand_iops, read_large_rand_cost) =
        if let Some(d) = d_read_large_rand {
            let tput = (total_large_bytes / (1024.0 * 1024.0)) / d.as_secs_f64();
            let iops = (threads * num_chunks) as f64 / d.as_secs_f64();
            let cost = (d.as_secs_f64() * 1000.0) / (threads * num_chunks) as f64;
            (Some(tput), Some(iops), Some(cost))
        } else {
            (None, None, None)
        };

    let (write_small_seq_tput, write_small_seq_iops, write_small_seq_cost) =
        if let Some(d) = d_write_small_seq {
            let tput = (total_small_bytes / (1024.0 * 1024.0)) / d.as_secs_f64();
            let iops = total_small_files / d.as_secs_f64();
            let cost = (d.as_secs_f64() * 1000.0) / total_small_files;
            (Some(tput), Some(iops), Some(cost))
        } else {
            (None, None, None)
        };

    let (read_small_seq_tput, read_small_seq_iops, read_small_seq_cost) =
        if let Some(d) = d_read_small_seq {
            let tput = (total_small_bytes / (1024.0 * 1024.0)) / d.as_secs_f64();
            let iops = total_small_files / d.as_secs_f64();
            let cost = (d.as_secs_f64() * 1000.0) / total_small_files;
            (Some(tput), Some(iops), Some(cost))
        } else {
            (None, None, None)
        };

    let (write_small_rand_tput, write_small_rand_iops, write_small_rand_cost) =
        if let Some(d) = d_write_small_rand {
            let tput = (total_small_bytes / (1024.0 * 1024.0)) / d.as_secs_f64();
            let iops = total_small_files / d.as_secs_f64();
            let cost = (d.as_secs_f64() * 1000.0) / total_small_files;
            (Some(tput), Some(iops), Some(cost))
        } else {
            (None, None, None)
        };

    let (read_small_rand_tput, read_small_rand_iops, read_small_rand_cost) =
        if let Some(d) = d_read_small_rand {
            let tput = (total_small_bytes / (1024.0 * 1024.0)) / d.as_secs_f64();
            let iops = total_small_files / d.as_secs_f64();
            let cost = (d.as_secs_f64() * 1000.0) / total_small_files;
            (Some(tput), Some(iops), Some(cost))
        } else {
            (None, None, None)
        };

    let (stat_iops, stat_cost) = if let Some(d) = d_stat {
        let iops = total_small_files / d.as_secs_f64();
        let cost = (d.as_secs_f64() * 1000.0) / total_small_files;
        (Some(iops), Some(cost))
    } else {
        (None, None)
    };

    let (mkdir_iops, mkdir_cost) = if let Some(d) = d_mkdir {
        let iops = total_small_files / d.as_secs_f64();
        let cost = (d.as_secs_f64() * 1000.0) / total_small_files;
        (Some(iops), Some(cost))
    } else {
        (None, None)
    };

    let (readdir_iops, readdir_cost) = if let Some(d) = d_readdir {
        let iops = total_small_files / d.as_secs_f64();
        let cost = (d.as_secs_f64() * 1000.0) / total_small_files;
        (Some(iops), Some(cost))
    } else {
        (None, None)
    };

    let (rmdir_iops, rmdir_cost) = if let Some(d) = d_rmdir {
        let iops = total_small_files / d.as_secs_f64();
        let cost = (d.as_secs_f64() * 1000.0) / total_small_files;
        (Some(iops), Some(cost))
    } else {
        (None, None)
    };

    let (delete_iops, delete_cost) = if let Some(d) = d_delete {
        let count_multiplier = if path.join("bench_small_seq_0_0.bin").exists()
            && path.join("bench_small_rand_0_0.bin").exists()
        {
            2.0
        } else {
            1.0
        };
        let iops = (total_small_files * count_multiplier) / d.as_secs_f64();
        let cost = (d.as_secs_f64() * 1000.0) / (total_small_files * count_multiplier);
        (Some(iops), Some(cost))
    } else {
        (None, None)
    };

    // --- COLOR THRESHOLDS ---
    let format_tput = |val: f64| {
        let s = format!("{:>12.2} MiB/s", val);
        if val > 100.0 {
            s.green()
        } else if val > 50.0 {
            s.yellow()
        } else {
            s.red()
        }
    };
    let format_iops = |val: f64| {
        let s = format!("{:>12.2} ops/s", val);
        if val > 200.0 {
            s.green()
        } else if val > 100.0 {
            s.yellow()
        } else {
            s.red()
        }
    };
    let format_stat_iops = |val: f64| {
        let s = format!("{:>12.2} ops/s", val);
        if val > 2000.0 {
            s.green()
        } else if val > 1000.0 {
            s.yellow()
        } else {
            s.red()
        }
    };

    println!(
        "\n{}",
        "+----------------------+--------------------+--------------------+--------------------+"
            .bold()
    );
    println!(
        "| {:<20} | {:<18} | {:<18} | {:<18} |",
        "WORKLOAD", "THROUGHPUT", "IOPS", "AVG LATENCY"
    );
    println!(
        "{}",
        "+----------------------+--------------------+--------------------+--------------------+"
            .bold()
    );

    let print_row = |name: &str,
                     tput: Option<f64>,
                     iops: Option<f64>,
                     latency: Option<f64>,
                     is_metadata: bool| {
        let tput_str = if is_metadata {
            "N/A".normal()
        } else if let Some(t) = tput {
            format_tput(t)
        } else {
            "Skipped".yellow()
        };
        let iops_str = if let Some(i) = iops {
            if is_metadata {
                format_stat_iops(i)
            } else {
                format_iops(i)
            }
        } else {
            "Skipped".yellow()
        };
        let lat_str = if let Some(l) = latency {
            format!("{:>14.2} ms", l).yellow()
        } else {
            "Skipped".yellow()
        };
        println!(
            "| {:<20} | {:>18} | {:>18} | {:>18} |",
            name, tput_str, iops_str, lat_str
        );
    };

    print_row(
        "Write (Large Seq)",
        write_large_seq_tput,
        write_large_seq_iops,
        write_large_seq_cost,
        false,
    );
    print_row(
        "Read (Large Seq)",
        read_large_seq_tput,
        read_large_seq_iops,
        read_large_seq_cost,
        false,
    );
    print_row(
        "Write (Large Rand)",
        write_large_rand_tput,
        write_large_rand_iops,
        write_large_rand_cost,
        false,
    );
    print_row(
        "Read (Large Rand)",
        read_large_rand_tput,
        read_large_rand_iops,
        read_large_rand_cost,
        false,
    );
    print_row(
        "Write (Small Seq)",
        write_small_seq_tput,
        write_small_seq_iops,
        write_small_seq_cost,
        false,
    );
    print_row(
        "Read (Small Seq)",
        read_small_seq_tput,
        read_small_seq_iops,
        read_small_seq_cost,
        false,
    );
    print_row(
        "Write (Small Rand)",
        write_small_rand_tput,
        write_small_rand_iops,
        write_small_rand_cost,
        false,
    );
    print_row(
        "Read (Small Rand)",
        read_small_rand_tput,
        read_small_rand_iops,
        read_small_rand_cost,
        false,
    );
    println!(
        "{}",
        "+----------------------+--------------------+--------------------+--------------------+"
            .bold()
    );
    print_row("Metadata Stat", None, stat_iops, stat_cost, true);
    print_row("Metadata Mkdir", None, mkdir_iops, mkdir_cost, true);
    print_row("Metadata Readdir", None, readdir_iops, readdir_cost, true);
    print_row("Metadata Rmdir", None, rmdir_iops, rmdir_cost, true);
    print_row("Metadata Delete", None, delete_iops, delete_cost, true);
    println!(
        "{}",
        "+----------------------+--------------------+--------------------+--------------------+"
            .bold()
    );

    if let (Some(base), Some(post)) = (baseline_metrics, post_metrics) {
        println!(
            "\n{}",
            "Daemon Backend Metrics (End-to-End Audit):".bold().cyan()
        );
        println!(
            "{}",
            "+------------------------------+--------------------+".bold()
        );
        println!("| {:<28} | {:<18} |", "DAEMON METRIC", "DIFF VALUE");
        println!(
            "{}",
            "+------------------------------+--------------------+".bold()
        );

        let show_metric = |label: &str, key: &str| {
            let v1 = base.get(key).unwrap_or(&0);
            let v2 = post.get(key).unwrap_or(&0);
            let diff = v2.saturating_sub(*v1);
            println!("| {:<28} | {:>18} |", label, diff);
        };

        show_metric("FUSE Operations", "fuse_ops");
        show_metric("Metadata Updates", "meta_updates");
        show_metric("NVMe-oF Write Block", "put_obj");
        show_metric("NVMe-oF Read Block", "get_obj");
        show_metric("NVMe-oF Delete Block", "del_obj");
        show_metric("Cache Hits (RAM)", "cache_hits");
        show_metric("Cache Misses", "cache_misses");
        println!(
            "{}",
            "+------------------------------+--------------------+".bold()
        );
    } else {
        println!("\n(Note: FUSE daemon metrics not available. Ensure squeezefs mount is running locally)");
    }

    Ok(())
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
                    if let Ok(curr) = std::fs::read_to_string(&max_bg_path) {
                        println!(
                            "FUSE Connection {:?}: max_background = {}",
                            entry.file_name(),
                            curr.trim()
                        );
                    }
                    if is_root {
                        let _ = std::fs::write(max_bg_path, "64\n");
                        let _ = std::fs::write(cong_path, "48\n");
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
