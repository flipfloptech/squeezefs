#![allow(clippy::all)]

use clap::{Parser, Subcommand};
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use redis::AsyncCommands;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{start_mount, SqueezefsFilesystem};
use squeezefs::routing::DataRouter;
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
    /// Format Garnet database to initialize squeezefs volume
    Format {
        /// SqueezeFS URI (squeeze://ip:port/filesystemname)
        squeeze_uri: String,
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
        /// SqueezeFS LVM Volume path (e.g. /dev/mypool/myvol)
        #[arg(long, alias = "backing-dev", alias = "nvme-target")]
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
        /// Optional SqueezeFS URI (squeeze://ip:port/filesystemname) or mount point path
        squeeze_uri: Option<String>,
    },
    /// List all active clients that have the filesystem mounted
    Clients {
        /// SqueezeFS URI (squeeze://ip:port/filesystemname)
        squeeze_uri: String,
    },
    /// Mount squeezefs at a target path
    Mount {
        /// SqueezeFS URI (squeeze://ip:port/filesystemname)
        squeeze_uri: String,
        /// Path to mount the filesystem at
        mountpoint: PathBuf,

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

        /// Comma-separated paths to local staging/cache directories
        #[arg(long, value_delimiter = ',', alias = "cache-dir")]
        disk_cache_paths: Option<Vec<PathBuf>>,

        /// Comma-separated list of local source IP interfaces for multi-rail connection bonding
        #[arg(long, value_delimiter = ',')]
        local_ips: Option<Vec<std::net::IpAddr>>,

        /// SqueezeFS LVM Volume path (e.g. /dev/mypool/myvol)
        #[arg(long, alias = "backing-dev", alias = "nvme-target")]
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
    /// Cleanly unmount a squeezefs mountpoint, with options to cancel, wait, or force dismount
    Umount {
        /// Optional SqueezeFS URI (squeeze://ip:port/filesystemname)
        #[arg(
            long,
            short = 'g',
            env = "GARNET_URL",
            alias = "squeeze-uri",
            alias = "squeeze_uri"
        )]
        squeeze_uri: Option<String>,
        /// Path to the mountpoint
        mountpoint: PathBuf,
        /// Force unmount immediately without prompting/waiting
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
        /// Optional SqueezeFS URI (squeeze://ip:port/filesystemname)
        #[arg(
            long,
            short = 'g',
            env = "GARNET_URL",
            alias = "squeeze-uri",
            alias = "squeeze_uri"
        )]
        squeeze_uri: Option<String>,
        /// Source file path
        src: String,
        /// Destination file path
        dest: String,
    },
    /// Defragment a formatted SqueezeFS volume
    Defrag {
        /// Optional SqueezeFS URI (squeeze://ip:port/filesystemname)
        #[arg(
            long,
            short = 'g',
            env = "GARNET_URL",
            alias = "squeeze-uri",
            alias = "squeeze_uri"
        )]
        squeeze_uri: Option<String>,
        /// NVMe device path
        #[arg(long)]
        nvme_path: String,
    },
    /// Automatically tune client node configurations (requires root/sudo to apply changes)
    Tune,
    /// Configuration management utility
    Config {
        /// Optional SqueezeFS URI (squeeze://ip:port/filesystemname)
        #[arg(
            long,
            short = 'g',
            env = "GARNET_URL",
            alias = "squeeze-uri",
            alias = "squeeze_uri"
        )]
        squeeze_uri: Option<String>,
        #[command(subcommand)]
        action: ConfigActions,
    },
    /// Show filesystem disk space usage across all caches and NVMe-oF backend
    Df {
        /// Optional SqueezeFS URI (squeeze://ip:port/filesystemname)
        #[arg(
            long,
            short = 'g',
            env = "GARNET_URL",
            alias = "squeeze-uri",
            alias = "squeeze_uri"
        )]
        squeeze_uri: Option<String>,
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
        /// Comma-separated list of local source IP interfaces for multi-rail connection
        #[arg(long, value_delimiter = ',')]
        local_ips: Option<Vec<std::net::IpAddr>>,
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
    /// Manage storage volumes
    #[command(subcommand, alias = "backends", alias = "backend", alias = "volumes")]
    Volume(VolumeActions),
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
enum VolumeActions {
    /// Add a storage volume
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
    /// Remove a storage volume
    Remove {
        /// Volume ID
        volume_id: String,
        /// Force removal ignoring safety checks
        #[arg(long)]
        force: bool,
    },
    /// List all storage volumes and their status
    List,
    /// Set the active write volume
    SetActive {
        /// Volume ID
        volume_id: String,
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

const FUSE_IO_URING_SQPOLL_IDLE_MS_KEY: &str = "fuse_io_uring_sqpoll_idle_ms";
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

fn resolve_shared_sqpoll_idle_ms(
    explicit_override: Option<u32>,
    format_fields: &HashMap<String, String>,
) -> Result<Option<u32>, String> {
    if let Some(value) = explicit_override {
        return Ok((value > 0).then_some(value));
    }

    match format_fields
        .get(FUSE_IO_URING_SQPOLL_IDLE_MS_KEY)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        Some(raw) => {
            let parsed = raw.parse::<u32>().map_err(|err| {
                format!(
                    "Invalid {} value '{}': {}",
                    FUSE_IO_URING_SQPOLL_IDLE_MS_KEY, raw, err
                )
            })?;
            Ok((parsed > 0).then_some(parsed))
        }
        None => Ok(None),
    }
}

fn resolve_local_sqpoll_cpu(explicit_override: Option<u32>) -> Option<u32> {
    explicit_override.filter(|value| *value > 0)
}

fn format_optional_u32(value: Option<u32>, disabled_label: &str) -> String {
    value
        .map(|parsed| parsed.to_string())
        .unwrap_or_else(|| disabled_label.to_string())
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "dhat-on")]
    let _profiler = dhat::Profiler::new_heap();

    let cli = Cli::parse();

    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            match &cli.command {
                Commands::Format { .. }
                | Commands::Mount { .. }
                | Commands::Umount { .. }
                | Commands::Config { .. }
                | Commands::Tune
                | Commands::Status { .. }
                | Commands::Clients { .. } => {
                    eprintln!("Error: This command must be run as root (or with sudo).");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
    }

    #[cfg(unix)]
    if let Commands::Mount {
        squeeze_uri,
        mountpoint,
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
        ..
    } = &cli.command
    {
        let (redis_url, fs_name) = match resolve_squeeze_uri(Some(squeeze_uri), None) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        };
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            if !mountpoint.exists() {
                eprintln!("Error: Mountpoint {:?} does not exist.", mountpoint);
                std::process::exit(1);
            }
            use std::os::unix::fs::MetadataExt;
            match std::fs::metadata(mountpoint) {
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
            &redis_url,
            mountpoint,
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
    garnet_url: &str,
    mountpoint: &Path,
    fs_name: &str,
    daemon: bool,
    mem_cache_size: Option<&str>,
    disk_cache_size: Option<&str>,
    read_cache_size: Option<&str>,
    write_cache_size: Option<&str>,
    read_mem_cache_size: Option<&str>,
    write_mem_cache_size: Option<&str>,
    disk_cache_paths: Option<&[PathBuf]>,
    writeback: bool,
    allow_other: bool,
    options: Option<&str>,
    fuse_io_uring_sqpoll_idle_ms: Option<u32>,
    fuse_io_uring_sqpoll_cpu: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    use redis::Commands;
    squeezefs::set_fs_prefix(fs_name);
    let client = redis::Client::open(garnet_url)?;
    let mut con = client.get_connection()?;
    let key = squeezefs::fs_key!("format");
    log::info!(
        "print_mount_diagnostics: Querying format key '{}' on Redis '{}'",
        key,
        garnet_url
    );
    let format_fields: std::collections::HashMap<String, String> = con.hgetall(&key)?;
    log::info!(
        "print_mount_diagnostics: Retrieved format fields: {:?}",
        format_fields
    );
    if format_fields.is_empty() {
        return Err("Volume not formatted. Please run format command first.".into());
    }

    let name = format_fields
        .get("name")
        .cloned()
        .unwrap_or_else(|| "unnamed".to_string());

    let block_size_bytes: u64 = format_fields
        .get("block_size")
        .and_then(|v| v.parse().ok())
        .unwrap_or(4194304);
    let block_size_str = format_size(block_size_bytes);

    let capacity_bytes: u64 = format_fields
        .get("capacity")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let capacity_str = if capacity_bytes == 0 {
        "unlimited".to_string()
    } else {
        format_size(capacity_bytes)
    };

    let inodes_limit: u64 = format_fields
        .get("inodes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let inodes_str = if inodes_limit == 0 {
        "0 (unlimited)".to_string()
    } else {
        inodes_limit.to_string()
    };

    let resolved_mem_cache_size = mem_cache_size
        .map(|s| s.to_string())
        .or_else(|| {
            format_fields
                .get("mem_cache_size")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| "1GB".to_string());

    let resolved_disk_cache_size = disk_cache_size
        .map(|s| s.to_string())
        .or_else(|| {
            format_fields
                .get("disk_cache_size")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| "10GB".to_string());

    let resolved_read_cache_size = read_cache_size
        .map(|s| s.to_string())
        .or_else(|| {
            format_fields
                .get("read_cache_size")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| halve_size_string(&resolved_disk_cache_size, "5GB"));

    let resolved_write_cache_size = write_cache_size
        .map(|s| s.to_string())
        .or_else(|| {
            format_fields
                .get("write_cache_size")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| halve_size_string(&resolved_disk_cache_size, "5GB"));

    let resolved_read_mem_cache_size = read_mem_cache_size
        .map(|s| s.to_string())
        .or_else(|| {
            format_fields
                .get("read_mem_cache_size")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| halve_size_string(&resolved_mem_cache_size, "512MB"));

    let resolved_write_mem_cache_size = write_mem_cache_size
        .map(|s| s.to_string())
        .or_else(|| {
            format_fields
                .get("write_mem_cache_size")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| halve_size_string(&resolved_mem_cache_size, "512MB"));

    let staging_dirs = if let Some(dirs) = disk_cache_paths {
        if dirs.is_empty() {
            vec![get_default_staging_dir()]
        } else if dirs.len() == 1
            && (dirs[0] == PathBuf::from("none")
                || dirs[0] == PathBuf::from("memory")
                || dirs[0] == PathBuf::from("memory-only")
                || dirs[0] == PathBuf::from(""))
        {
            Vec::new()
        } else {
            dirs.to_vec()
        }
    } else if let Some(paths_str) = format_fields.get("disk_cache_paths") {
        if paths_str.is_empty() || paths_str == "none" || paths_str == "memory" {
            Vec::new()
        } else {
            paths_str.split(',').map(PathBuf::from).collect()
        }
    } else {
        vec![get_default_staging_dir()]
    };

    let active_be_id = format_fields
        .get("active_write_backend")
        .cloned()
        .unwrap_or_else(|| "backend_0".to_string());

    let resolved_sqpoll_idle_ms =
        resolve_shared_sqpoll_idle_ms(fuse_io_uring_sqpoll_idle_ms, &format_fields)
            .map_err(|msg| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))?;
    let resolved_sqpoll_cpu = resolve_local_sqpoll_cpu(fuse_io_uring_sqpoll_cpu);

    let compression = format_fields
        .get("compression")
        .cloned()
        .unwrap_or_else(|| "none".to_string());
    let encrypt_algo = format_fields
        .get("encrypt_algo")
        .cloned()
        .unwrap_or_else(|| "none".to_string());
    let encrypt_key_present = format_fields
        .get("encrypt_key")
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let masked_encrypt_key = if encrypt_key_present {
        "******".to_string()
    } else {
        "none".to_string()
    };

    println!("SqueezeFS version {}", env!("CARGO_PKG_VERSION"));
    println!("===================================================");
    println!("Mount Options:");
    println!("  Mountpoint: {:?}", mountpoint);
    println!("  Daemon: {}", daemon);
    println!("  Writeback: {}", writeback);
    let resolved_max_uploads = std::cmp::max(
        16,
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(8)
            * 2,
    );
    println!("  Max Background Uploads: {}", resolved_max_uploads);
    println!("  Allow Other: {}", allow_other);
    if let Some(opts) = options {
        println!("  Custom Options: {:?}", opts);
    } else {
        println!("  Custom Options: \"max_read=1048576\"");
    }
    println!("I/O Rings:");
    println!(
        "  FUSE io_uring SQPOLL Idle (ms): {}",
        format_optional_u32(resolved_sqpoll_idle_ms, "disabled")
    );
    println!(
        "  FUSE io_uring SQPOLL CPU: {}",
        format_optional_u32(resolved_sqpoll_cpu, "auto")
    );
    println!("Metadata Client:");
    println!("  Garnet URL: {:?}", garnet_url);
    println!("  Volume Name: {:?}", name);
    println!("  Block Size: {}", block_size_str);
    println!("  Capacity: {}", capacity_str);
    println!("  Inodes Limit: {}", inodes_str);
    println!("Cache Settings:");
    println!("  Memory Cache Size (Total): {}", resolved_mem_cache_size);
    println!(
        "    Read Memory Cache Size:  {}",
        resolved_read_mem_cache_size
    );
    println!(
        "    Write Memory Cache Size: {}",
        resolved_write_mem_cache_size
    );
    println!("  Disk Cache Size (Total):   {}", resolved_disk_cache_size);
    println!("    Read Disk Cache Size:    {}", resolved_read_cache_size);
    println!("    Write Disk Cache Size:   {}", resolved_write_cache_size);
    println!("  Disk Cache Paths: {:?}", staging_dirs);
    println!("Storage Backend:");
    println!("  Active Backend: {:?}", active_be_id);
    println!("Security & Compression:");
    println!("  Compression: {:?}", compression);
    println!("  Encryption Algorithm: {:?}", encrypt_algo);
    println!("  Encryption Key: {}", masked_encrypt_key);
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
            squeeze_uri,
            block_size,
            capacity,
            mem_cache_size,
            disk_cache_size,
            disk_cache_paths,
            volume: backing_dev,
            ip,
            port,
            subnqn,
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
        } => {
            let (redis_url, name) = resolve_squeeze_uri(Some(&squeeze_uri), None)?;
            squeezefs::set_fs_prefix(&name);
            let _ctrl_c_guard = spawn_ctrl_c_handler("formatting");
            let quick = !full;

            // Fail-fast connection to Redis/Garnet
            let client = redis::Client::open(redis_url.as_str())
                .map_err(|e| format!("Failed to open Redis client at {}: {:?}", redis_url, e))?;
            let mut con = client
                .get_multiplexed_tokio_connection()
                .await
                .map_err(|e| {
                    format!(
                        "Failed to connect to metadata database at {}: {:?}",
                        redis_url, e
                    )
                })?;

            // Check if active clients are connected to the filesystem
            let raw_clients: std::collections::HashMap<String, String> = con
                .hgetall(squeezefs::fs_key!("active_clients"))
                .await
                .unwrap_or_default();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut active_clients = Vec::new();
            for (_, json_str) in raw_clients {
                if let Ok(info) =
                    serde_json::from_str::<squeezefs::fuse_client::ClientInfo>(&json_str)
                {
                    if now.saturating_sub(info.last_heartbeat) <= 6 {
                        active_clients.push(info);
                    }
                }
            }
            if !active_clients.is_empty() {
                use colored::Colorize;
                println!(
                    "{}",
                    "ERROR: Cannot format filesystem because active clients are connected:"
                        .red()
                        .bold()
                );
                for client in active_clients {
                    println!(
                        "  - Client ID: {} | Host: {} | PID: {} | Mount: {}",
                        client.client_id, client.hostname, client.pid, client.mountpoint
                    );
                }
                return Err("Active clients are connected".into());
            }

            // Check if squeezefs volume is already formatted on the database
            if !force {
                let key = squeezefs::fs_key!("format");
                log::info!(
                    "Format command: checking if key '{}' exists on Redis '{}'",
                    key,
                    redis_url
                );
                let exists_format: bool = con.exists(&key).await.unwrap_or(false);
                if exists_format {
                    println!(
                        "{}",
                        "WARNING: A squeezefs volume is already formatted on this database."
                            .yellow()
                            .bold()
                    );
                    println!(
                        "{}",
                        "Formatting will delete all existing metadata and files!"
                            .yellow()
                            .bold()
                    );
                    print!("Are you sure you want to proceed? [y/N]: ");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    let mut input = String::new();
                    let _ = std::io::stdin().read_line(&mut input);
                    let trimmed = input.trim().to_lowercase();
                    if trimmed != "y" && trimmed != "yes" {
                        println!("Format aborted.");
                        return Ok(());
                    }
                }
            }

            let resolved_encrypt_key_pem = if encrypt_algo != "none" {
                use rsa::pkcs1::EncodeRsaPrivateKey;
                if let Some(ref path_str) = encrypt_key {
                    let path = std::path::Path::new(path_str);
                    let pem = std::fs::read_to_string(path)?;
                    Some(pem)
                } else {
                    println!("No --encrypt-key provided. Automatically generating a new 2048-bit RSA key pair...");
                    let mut rng = rand::thread_rng();
                    let priv_key = rsa::RsaPrivateKey::new(&mut rng, 2048)
                        .map_err(|e| format!("Failed to generate RSA key: {}", e))?;
                    let pem = priv_key
                        .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
                        .map_err(|e| format!("Failed to format PEM: {}", e))?;
                    std::fs::write("squeezefs.key", &*pem)?;
                    println!("Successfully generated squeezefs.key file.");
                    Some((*pem).clone())
                }
            } else {
                None
            };

            // 2. Compression algorithm check
            let comp = compression.to_lowercase();
            if comp != "none" && comp != "lz4" && comp != "zstd" && !comp.is_empty() {
                return Err(format!(
                    "Unsupported compression algorithm: '{}'. Supported options are: none, lz4, zstd.",
                    compression
                ).into());
            }

            // 3. Encryption algorithm check
            let enc = encrypt_algo.to_lowercase();
            if enc != "none" && enc != "aes256gcm-rsa" && enc != "chacha20-rsa" && !enc.is_empty() {
                return Err(format!(
                    "Unsupported encryption algorithm: '{}'. Supported options are: none, aes256gcm-rsa, chacha20-rsa.",
                    encrypt_algo
                ).into());
            }

            // 5. Human readable size limits validation
            if let Some(ref sz) = mem_cache_size
                .as_ref()
                .filter(|s| !s.is_empty() && *s != "none")
            {
                squeezefs::cache::parse_size_string(sz, 1024 * 1024)
                    .map_err(|e| format!("Invalid mem_cache_size '{}': {:?}", sz, e))?;
            }
            if let Some(ref sz) = disk_cache_size
                .as_ref()
                .filter(|s| !s.is_empty() && *s != "none")
            {
                squeezefs::cache::parse_size_string(sz, 1024 * 1024)
                    .map_err(|e| format!("Invalid disk_cache_size '{}': {:?}", sz, e))?;
            }
            if let Some(ref sz) = read_cache_size
                .as_ref()
                .filter(|s| !s.is_empty() && *s != "none")
            {
                squeezefs::cache::parse_size_string(sz, 1024 * 1024)
                    .map_err(|e| format!("Invalid read_cache_size '{}': {:?}", sz, e))?;
            }
            if let Some(ref sz) = write_cache_size
                .as_ref()
                .filter(|s| !s.is_empty() && *s != "none")
            {
                squeezefs::cache::parse_size_string(sz, 1024 * 1024)
                    .map_err(|e| format!("Invalid write_cache_size '{}': {:?}", sz, e))?;
            }
            if let Some(ref sz) = read_mem_cache_size
                .as_ref()
                .filter(|s| !s.is_empty() && *s != "none")
            {
                squeezefs::cache::parse_size_string(sz, 1024 * 1024)
                    .map_err(|e| format!("Invalid read_mem_cache_size '{}': {:?}", sz, e))?;
            }
            if let Some(ref sz) = write_mem_cache_size
                .as_ref()
                .filter(|s| !s.is_empty() && *s != "none")
            {
                squeezefs::cache::parse_size_string(sz, 1024 * 1024)
                    .map_err(|e| format!("Invalid write_mem_cache_size '{}': {:?}", sz, e))?;
            }

            // 6. Dismount wait validation
            if let Some(ref wait) = dismount_wait.as_ref().filter(|s| !s.is_empty()) {
                wait.parse::<u64>()
                    .map_err(|e| format!("Invalid dismount_wait '{}': {:?}", wait, e))?;
            }

            let parsed_block_size = parse_human_readable_size(&block_size)?;

            squeezefs::cache::parse_duration(&upload_delay)?;

            let mut resolved_backing_dev = backing_dev.clone();

            // Check if format already exists
            use redis::AsyncCommands;
            let existing_format = con
                .hgetall::<_, std::collections::HashMap<String, String>>(squeezefs::fs_key!(
                    "format"
                ))
                .await
                .unwrap_or_default();

            let parsed_capacity = if let Some(ref cap_str) = capacity {
                parse_human_readable_size(cap_str)?
            } else {
                let backing_path = backing_dev
                    .as_ref()
                    .or_else(|| existing_format.get("backing_dev"))
                    .map(|s| s.as_str());

                if let Some(path) = backing_path {
                    if std::path::Path::new(path).exists() {
                        match get_backing_device_size(path) {
                            Ok(size) if size > 0 => {
                                println!(
                                    "Auto-detected volume capacity: {}",
                                    format_size_human(size)
                                );
                                size
                            }
                            _ => parse_human_readable_size("1P")?,
                        }
                    } else {
                        parse_human_readable_size("1P")?
                    }
                } else {
                    parse_human_readable_size("1P")?
                }
            };

            let mut resolved_ip = ip.clone();
            let mut resolved_port = port;
            let mut resolved_subnqn = subnqn.clone();

            if !existing_format.is_empty() {
                // Pull everything from the existing format metadata if not explicitly provided
                if resolved_backing_dev.is_none() {
                    resolved_backing_dev = existing_format
                        .get("backing_dev")
                        .filter(|s| !s.is_empty())
                        .cloned();
                }
                if resolved_ip.is_none() {
                    resolved_ip = existing_format
                        .get("backing_dev_ip")
                        .filter(|s| !s.is_empty())
                        .cloned();
                }
                if resolved_port.is_none() {
                    resolved_port = existing_format
                        .get("backing_dev_port")
                        .and_then(|s| s.parse::<u16>().ok());
                }
                if resolved_subnqn.is_none() {
                    resolved_subnqn = existing_format
                        .get("backing_dev_subnqn")
                        .filter(|s| !s.is_empty())
                        .cloned();
                }
            }

            let resolved_backing_dev = match resolved_backing_dev {
                Some(path) => {
                    // Check if it is the in-memory testing path. If it is and does not exist, recreate it.
                    if (path.starts_with("/dev/shm/") || path.starts_with("/tmp/"))
                        && !std::path::Path::new(&path).exists()
                    {
                        log::info!("Re-initializing testing backing device file at {}...", path);
                        let file = std::fs::File::create(&path).map_err(|e| {
                            format!(
                                "Failed to create backing device file at '{}': {:?}",
                                path, e
                            )
                        })?;
                        file.set_len(parsed_capacity).map_err(|e| {
                            format!(
                                "Failed to set size of backing device file at '{}': {:?}",
                                path, e
                            )
                        })?;
                    } else {
                        // Validate loop / LVM / NVMe
                        squeezefs::storage::validate_backing_device(&path)?;
                    }

                    // Automatically pull NVMe-oF details if possible
                    if let Some((ext_ip, ext_port, ext_subnqn)) =
                        squeezefs::nvmeof::extract_nvmeof_connection_details(&path)
                    {
                        log::info!(
                            "Automatically extracted NVMe-oF connection details for {}: {}:{} / {}",
                            path,
                            ext_ip,
                            ext_port,
                            ext_subnqn
                        );
                        if resolved_ip.is_none() {
                            resolved_ip = Some(ext_ip);
                        }
                        if resolved_port.is_none() {
                            resolved_port = Some(ext_port);
                        }
                        if resolved_subnqn.is_none() {
                            resolved_subnqn = Some(ext_subnqn);
                        }
                    }
                    path
                }
                None => {
                    if let (Some(ref ip_val), Some(port_val), Some(ref nqn_val)) =
                        (&resolved_ip, resolved_port, &resolved_subnqn)
                    {
                        log::info!(
                            "Connecting to NVMe-oF target at {}:{} / {}...",
                            ip_val,
                            port_val,
                            nqn_val
                        );
                        let dev_path = squeezefs::nvmeof::connect_target(ip_val, port_val, nqn_val)
                            .map_err(|e| format!("Failed to connect to NVMe-oF target: {:?}", e))?;
                        log::info!("Connected to remote NVMe-oF disk: {}", dev_path);
                        // Also auto-extract details just in case
                        if let Some((ext_ip, ext_port, ext_subnqn)) =
                            squeezefs::nvmeof::extract_nvmeof_connection_details(&dev_path)
                        {
                            if resolved_ip.is_none() {
                                resolved_ip = Some(ext_ip);
                            }
                            if resolved_port.is_none() {
                                resolved_port = Some(ext_port);
                            }
                            if resolved_subnqn.is_none() {
                                resolved_subnqn = Some(ext_subnqn);
                            }
                        }
                        dev_path
                    } else {
                        return Err(format!(
                            "Error: No backing device specified for the first format. \
                            Please ensure that the backing device is created, online, and specified \
                            via --volume before the first format."
                        ).into());
                    }
                }
            };

            squeezefs::fuse_client::format_volume_ext(
                &redis_url,
                &name,
                parsed_block_size,
                parsed_capacity,
                inodes,
                &compression,
                &encrypt_algo,
                resolved_encrypt_key_pem.as_deref(),
                mem_cache_size.as_deref(),
                disk_cache_size.as_deref(),
                disk_cache_paths.as_deref(),
                Some(&resolved_backing_dev),
                resolved_ip.as_deref(),
                resolved_port,
                resolved_subnqn.as_deref(),
                read_cache_size.as_deref(),
                write_cache_size.as_deref(),
                read_mem_cache_size.as_deref(),
                write_mem_cache_size.as_deref(),
                dismount_wait.as_deref(),
                Some(&upload_delay),
                fuse_io_uring_sqpoll_idle_ms,
                quick,
            )
            .await?;
            let status = squeezefs::fuse_client::get_volume_status(&redis_url).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Commands::Status { squeeze_uri } => {
            let ref_path = squeeze_uri.as_ref().map(|p| std::path::Path::new(p));
            let (redis_url, fs_name) = resolve_squeeze_uri(squeeze_uri.as_deref(), ref_path)?;
            squeezefs::set_fs_prefix(&fs_name);
            let status = squeezefs::fuse_client::get_volume_status(&redis_url).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Commands::Clients { squeeze_uri } => {
            let (redis_url, name) = resolve_squeeze_uri(Some(&squeeze_uri), None)?;
            squeezefs::set_fs_prefix(&name);
            let client = squeezefs::dlm::MetaClient::new(&redis_url)?;
            let mut con = client.get_connection().await?;
            let raw_clients: std::collections::HashMap<String, String> =
                con.hgetall(squeezefs::fs_key!("active_clients")).await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut active_clients = Vec::new();
            for (_, json_str) in raw_clients {
                if let Ok(info) =
                    serde_json::from_str::<squeezefs::fuse_client::ClientInfo>(&json_str)
                {
                    if now.saturating_sub(info.last_heartbeat) <= 6 {
                        active_clients.push(info);
                    }
                }
            }
            if active_clients.is_empty() {
                println!("No active clients connected.");
            } else {
                println!("Active clients connected to volume '{}':", name);
                for client in active_clients {
                    println!(
                        "  - Client ID: {} | Host: {} | PID: {} | Mountpoint: {}",
                        client.client_id, client.hostname, client.pid, client.mountpoint
                    );
                }
            }
        }
        Commands::Mount {
            squeeze_uri,
            mountpoint,
            mem_cache_size,
            disk_cache_size,
            disk_cache_paths,
            volume: backing_dev,
            ip,
            port,
            subnqn,
            local_ips,
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
            dismount_wait,
            upload_delay,
            fuse_io_uring_sqpoll_idle_ms,
            fuse_io_uring_sqpoll_cpu,
            job_cpu_limit,
            write_verification,
            write_verification_sample,
        } => {
            let (redis_url_str, fs_name) = resolve_squeeze_uri(Some(&squeeze_uri), None)?;
            squeezefs::set_fs_prefix(&fs_name);
            squeezefs::set_write_verification(write_verification);
            squeezefs::set_write_verification_sample_rate(write_verification_sample);
            let writeback = !no_writeback;
            let redis_url = &redis_url_str;

            log::info!(
                "Connecting to Garnet (metadata database) at {}...",
                redis_url
            );
            let dlm =
                DlmClient::new_with_local_ips(redis_url, local_ips.clone().unwrap_or_default())
                    .await?;
            log::info!("Successfully connected to Garnet metadata database.");

            // Retrieve configuration settings from Garnet metadata if formatted
            let format_fields: std::collections::HashMap<String, String> = {
                if let Ok(mut con) = dlm.meta_client().get_connection().await {
                    con.hgetall(squeezefs::fs_key!("format"))
                        .await
                        .unwrap_or_default()
                } else {
                    std::collections::HashMap::new()
                }
            };

            // Resolve memory cache size: CLI override > Garnet setting > default "1GB"
            let resolved_mem_cache_size = mem_cache_size
                .or_else(|| {
                    format_fields
                        .get("mem_cache_size")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| "1GB".to_string());

            // Resolve dismount wait time: CLI override > Garnet setting > default 10 seconds
            let resolved_dismount_wait: u64 = dismount_wait
                .or_else(|| {
                    format_fields
                        .get("dismount_wait")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .and_then(|s| s.parse().ok())
                .unwrap_or(10);

            // If upload_delay was specified as a CLI override, persist it in Garnet
            if let Some(ref delay) = upload_delay {
                squeezefs::cache::parse_duration(delay)?;
                if let Ok(mut con) = dlm.meta_client().get_connection().await {
                    let _: Result<(), redis::RedisError> = redis::cmd("HSET")
                        .arg(squeezefs::fs_key!("format"))
                        .arg("upload_delay")
                        .arg(delay)
                        .query_async(&mut con)
                        .await;
                }
            }

            // Resolve upload delay: CLI override > Garnet setting > default "500ms"
            let resolved_upload_delay = upload_delay
                .or_else(|| {
                    format_fields
                        .get("upload_delay")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| "500ms".to_string());

            // Validate resolved upload delay
            squeezefs::cache::parse_duration(&resolved_upload_delay)?;

            let resolved_fuse_io_uring_sqpoll_idle_ms =
                resolve_shared_sqpoll_idle_ms(fuse_io_uring_sqpoll_idle_ms, &format_fields)
                    .map_err(|msg| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))?;
            let resolved_fuse_io_uring_sqpoll_cpu =
                resolve_local_sqpoll_cpu(fuse_io_uring_sqpoll_cpu);

            // Resolve disk cache size: CLI override > Garnet setting > default "10GB"
            let resolved_disk_cache_size = disk_cache_size
                .or_else(|| {
                    format_fields
                        .get("disk_cache_size")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| "10GB".to_string());

            let resolved_read_cache_size = read_cache_size
                .or_else(|| {
                    format_fields
                        .get("read_cache_size")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| halve_size_string(&resolved_disk_cache_size, "5GB"));

            let resolved_write_cache_size = write_cache_size
                .or_else(|| {
                    format_fields
                        .get("write_cache_size")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| halve_size_string(&resolved_disk_cache_size, "5GB"));

            let resolved_read_mem_cache_size = read_mem_cache_size
                .or_else(|| {
                    format_fields
                        .get("read_mem_cache_size")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| halve_size_string(&resolved_mem_cache_size, "512MB"));

            let resolved_write_mem_cache_size = write_mem_cache_size
                .or_else(|| {
                    format_fields
                        .get("write_mem_cache_size")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| halve_size_string(&resolved_mem_cache_size, "512MB"));

            // Resolve staging directories: CLI override > Garnet setting > default "/tmp/squeezefs_staging"
            let staging_dirs = if let Some(dirs) = disk_cache_paths {
                if dirs.is_empty() {
                    vec![get_default_staging_dir()]
                } else if dirs.len() == 1
                    && (dirs[0] == PathBuf::from("none")
                        || dirs[0] == PathBuf::from("memory")
                        || dirs[0] == PathBuf::from("memory-only")
                        || dirs[0] == PathBuf::from(""))
                {
                    Vec::new()
                } else {
                    dirs
                }
            } else if let Some(paths_str) = format_fields.get("disk_cache_paths") {
                if paths_str.is_empty() || paths_str == "none" || paths_str == "memory" {
                    Vec::new()
                } else {
                    paths_str.split(',').map(PathBuf::from).collect()
                }
            } else {
                vec![get_default_staging_dir()]
            };

            let mut active_staging_dirs = Vec::new();
            if let Ok(mut con) = dlm.meta_client().get_connection().await {
                let status_map: std::collections::HashMap<String, String> = con
                    .hgetall(squeezefs::fs_key!("diskcache:status"))
                    .await
                    .unwrap_or_default();
                for dir in staging_dirs {
                    let dir_str = dir.to_string_lossy().to_string();
                    let status = status_map
                        .get(&dir_str)
                        .map(|s| s.as_str())
                        .unwrap_or("enabled");
                    if status != "disabled" {
                        active_staging_dirs.push(dir);
                    }
                }
            } else {
                active_staging_dirs = staging_dirs;
            }

            let fs_name = format_fields
                .get("name")
                .cloned()
                .unwrap_or_else(|| "squeezefs".to_string());

            let sanitized_mount = mountpoint
                .to_string_lossy()
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect::<String>();
            // Clean up consecutive underscores
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

                // Create the parent directory for the isolated staging dir
                fs::create_dir_all(&isolated_dir).await?;
                // Create the shared cache directory if not exists
                fs::create_dir_all(&shared_cache_dir).await?;

                let symlink_path = isolated_dir.join("cache_segment");
                // Remove pre-existing cache file/symlink/directory if any
                let metadata = fs::symlink_metadata(&symlink_path).await;
                if let Ok(meta) = metadata {
                    if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                        fs::remove_dir_all(&symlink_path).await?;
                    } else {
                        fs::remove_file(&symlink_path).await?;
                    }
                }

                // Create symbolic link pointing to ../cache_segment
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink("../cache_segment", &symlink_path)?;
                }

                isolated_staging_dirs.push(isolated_dir);
            }
            active_staging_dirs = isolated_staging_dirs;

            // Tune host parameters automatically
            let _ = tune_system();

            log::info!("Initializing NVMe-oF Block Allocator and Block Device...");
            let block_alloc = std::sync::Arc::new(
                squeezefs::block_allocator::BlockAllocator::new(
                    std::sync::Arc::new(dlm.meta_client().clone()),
                    &fs_name,
                )
                .await?,
            );

            // Resolve backing device path: CLI override > Garnet format setting > default to in-memory testing backend
            let mut resolved_backing_dev = backing_dev
                .or_else(|| {
                    format_fields
                        .get("backing_dev")
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| "/dev/shm/squeezefs_default_backend".to_string());

            // If backing device does not exist, check if we can connect to its NVMe-oF target
            if !std::path::Path::new(&resolved_backing_dev).exists() {
                let resolved_ip = ip.clone().or_else(|| {
                    format_fields
                        .get("backing_dev_ip")
                        .filter(|s| !s.is_empty())
                        .cloned()
                });
                let resolved_port = port.or_else(|| {
                    format_fields
                        .get("backing_dev_port")
                        .and_then(|v| v.parse::<u16>().ok())
                });
                let resolved_subnqn = subnqn.clone().or_else(|| {
                    format_fields
                        .get("backing_dev_subnqn")
                        .filter(|s| !s.is_empty())
                        .cloned()
                });

                if let (Some(ip_val), Some(port_val), Some(nqn_val)) =
                    (resolved_ip, resolved_port, resolved_subnqn)
                {
                    log::info!("Backing device {} not found. Connecting to NVMe-oF target at {}:{} / {}...", resolved_backing_dev, ip_val, port_val, nqn_val);
                    if let Ok(dev_path) =
                        squeezefs::nvmeof::connect_target(&ip_val, port_val, &nqn_val)
                    {
                        log::info!("Connected to remote NVMe-oF disk: {}", dev_path);
                        resolved_backing_dev = dev_path;
                    }
                }
            }

            // If the backing device is an LVM logical volume built on loop physical volumes (flat files), auto-rebind and activate them
            let lvm_vg = format_fields
                .get("lvm_vg")
                .filter(|s| !s.is_empty())
                .map(|s| s.as_str());
            let lvm_loops_str = format_fields.get("lvm_loops").filter(|s| !s.is_empty());
            let lvm_loops = lvm_loops_str
                .and_then(|s| {
                    serde_json::from_str::<std::collections::HashMap<String, String>>(s).ok()
                })
                .unwrap_or_default();
            let _ = squeezefs::storage::restore_lvm_loop_devices(lvm_vg, &lvm_loops);

            // If the backing device path is in /dev/shm or /tmp and does not exist (e.g. after reboot), auto-recreate it
            if (resolved_backing_dev.starts_with("/dev/shm/")
                || resolved_backing_dev.starts_with("/tmp/"))
                && !std::path::Path::new(&resolved_backing_dev).exists()
            {
                let capacity_str = format_fields
                    .get("capacity")
                    .cloned()
                    .unwrap_or_else(|| "1G".to_string());
                let capacity_bytes =
                    squeezefs::cache::parse_size_string(&capacity_str, 1024 * 1024 * 1024)
                        .unwrap_or(1024 * 1024 * 1024);
                log::info!(
                    "Re-initializing in-memory backing device file at {} with capacity {}...",
                    resolved_backing_dev,
                    capacity_str
                );
                if let Ok(file) = std::fs::File::create(&resolved_backing_dev) {
                    let _ = file.set_len(capacity_bytes);
                }
            }

            squeezefs::storage::validate_backing_device(&resolved_backing_dev)?;

            log::info!("Backing Block Device: {}", resolved_backing_dev);
            let nvme_dev = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
                &resolved_backing_dev,
            ));

            // Read the full format values from Garnet for JSON logging
            let block_size_bytes: u64 = format_fields
                .get("block_size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(4194304);
            let capacity_bytes: u64 = format_fields
                .get("capacity")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let inodes_limit: u64 = format_fields
                .get("inodes")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let compression = format_fields
                .get("compression")
                .cloned()
                .unwrap_or_else(|| "none".to_string());
            let encrypt_algo = format_fields
                .get("encrypt_algo")
                .cloned()
                .unwrap_or_else(|| "none".to_string());
            let encrypt_key_present = format_fields
                .get("encrypt_key")
                .map(|s| !s.is_empty())
                .unwrap_or(false);

            let masked_encrypt_key = if encrypt_key_present {
                "******".to_string()
            } else {
                "none".to_string()
            };

            let config_json = serde_json::json!({
                "meta_url": redis_url,
                "volume_name": format_fields.get("name").cloned().unwrap_or_else(|| "unnamed".to_string()),
                "block_size": format_size(block_size_bytes),
                "capacity": if capacity_bytes == 0 { "unlimited".to_string() } else { format_size(capacity_bytes) },
                "inodes_limit": if inodes_limit == 0 { "0 (unlimited)".to_string() } else { inodes_limit.to_string() },
                "mem_cache_size": resolved_mem_cache_size,
                "read_mem_cache_size": resolved_read_mem_cache_size,
                "write_mem_cache_size": resolved_write_mem_cache_size,
                "disk_cache_size": resolved_disk_cache_size,
                "read_cache_size": resolved_read_cache_size,
                "write_cache_size": resolved_write_cache_size,
                "disk_cache_paths": active_staging_dirs.iter().map(|d| d.to_string_lossy()).collect::<Vec<_>>(),
                "dismount_wait_seconds": resolved_dismount_wait,
                "upload_delay": resolved_upload_delay,
                "fuse_io_uring_sqpoll_idle_ms": resolved_fuse_io_uring_sqpoll_idle_ms,
                "fuse_io_uring_sqpoll_cpu": resolved_fuse_io_uring_sqpoll_cpu,
                "writeback": writeback,
                "allow_other": allow_other,
                "options": options,
                "compression": compression,
                "encrypt_algo": encrypt_algo,
                "encrypt_key": masked_encrypt_key,
            });
            let config_str = serde_json::to_string_pretty(&config_json).unwrap_or_default();
            log::info!("SqueezeFS version {}", env!("CARGO_PKG_VERSION"));
            log::info!("SqueezeFS mount configuration:\n{}", config_str);

            // Run staging / active write recovery on mount startup
            for dir in &active_staging_dirs {
                log::info!("Running staging recovery on: {:?}", dir);
                let _recovery_count = squeezefs::recovery::recover_staging(
                    dir,
                    dlm.meta_client(),
                    &block_alloc,
                    &nvme_dev,
                )
                .await
                .unwrap_or_else(|e| {
                    log::warn!("Background recovery failed for dir {:?}: {}", dir, e);
                    0
                });
            }

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

            // Load supplementary backends from Garnet
            if let Ok(mut con) = dlm.meta_client().get_connection().await {
                use redis::AsyncCommands;
                let backends_raw: std::collections::HashMap<String, String> = con
                    .hgetall(squeezefs::fs_key!("backends"))
                    .await
                    .unwrap_or_default();
                for (be_id, be_json) in backends_raw {
                    if let Ok(config) = serde_json::from_str::<serde_json::Value>(&be_json) {
                        if let Some(bd) = config["backing_dev"].as_str() {
                            let mut resolved_bd = bd.to_string();
                            if !std::path::Path::new(&resolved_bd).exists() {
                                let ip_val = config["ip"].as_str();
                                let port_val = config["port"].as_u64().map(|p| p as u16);
                                let nqn_val = config["subnqn"].as_str();
                                if let (Some(ip), Some(port), Some(subnqn)) =
                                    (ip_val, port_val, nqn_val)
                                {
                                    log::info!("Supplementary backend '{}' device not found. Connecting to NVMe-oF target at {}:{} / {}...", be_id, ip, port, subnqn);
                                    if let Ok(dev_path) =
                                        squeezefs::nvmeof::connect_target(ip, port, subnqn)
                                    {
                                        log::info!(
                                            "Connected supplementary backend '{}' to: {}",
                                            be_id,
                                            dev_path
                                        );
                                        resolved_bd = dev_path;
                                    }
                                }
                            }
                            let lvm_vg = config["lvm_vg"].as_str();
                            let lvm_loops = config["lvm_loops"]
                                .as_object()
                                .map(|obj| {
                                    obj.iter()
                                        .map(|(k, v)| {
                                            (k.clone(), v.as_str().unwrap_or_default().to_string())
                                        })
                                        .collect::<std::collections::HashMap<String, String>>()
                                })
                                .unwrap_or_default();
                            let _ =
                                squeezefs::storage::restore_lvm_loop_devices(lvm_vg, &lvm_loops);
                            let _resolved_cap = config["capacity"]
                                .as_u64()
                                .unwrap_or(1024 * 1024 * 1024 * 1024);
                            let device = squeezefs::nvme_dev::NvmeBlockDev::new(&resolved_bd);
                            let dev_arc = std::sync::Arc::new(device);
                            let be_alloc_name = format!("{}:{}", fs_name, be_id);
                            if let Ok(allocator) = squeezefs::block_allocator::BlockAllocator::new(
                                std::sync::Arc::new(dlm.meta_client().clone()),
                                &be_alloc_name,
                            )
                            .await
                            {
                                router.backend_router.backends.insert(
                                    be_id,
                                    std::sync::Arc::new(squeezefs::routing::StorageBackend {
                                        device: dev_arc,
                                        block_allocator: std::sync::Arc::new(allocator),
                                    }),
                                );
                            }
                        }
                    }
                }

                // Set active write backend
                let active_be: String = con
                    .hget(squeezefs::fs_key!("format"), "active_write_backend")
                    .await
                    .unwrap_or(Some("backend_0".to_string()))
                    .unwrap_or_else(|| "backend_0".to_string());
                router
                    .backend_router
                    .active_write_backend
                    .store(std::sync::Arc::new(active_be));
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
            fs_engine.dismount_wait = resolved_dismount_wait;

            squeezefs::jobs::start_job_worker(
                std::sync::Arc::new(fs_engine.router.clone()),
                fs_name.clone(),
                job_cpu_limit,
            );

            apply_fuse_io_uring_sqpoll_env(
                resolved_fuse_io_uring_sqpoll_idle_ms,
                resolved_fuse_io_uring_sqpoll_cpu,
            );

            println!("Mounting Squeezefs at {:?}...", mountpoint);

            // Daemonization has already happened at the start of main() prior to Tokio runtime initialization.

            let ca_cert_hex = format_fields.get("ca_cert");
            let ca_key_hex = format_fields.get("ca_key");

            let ca_cert = ca_cert_hex.and_then(|hex| {
                let mut bytes = Vec::new();
                for i in (0..hex.len()).step_by(2) {
                    if let Ok(b) = u8::from_str_radix(&hex[i..i + 2], 16) {
                        bytes.push(b);
                    } else {
                        return None;
                    }
                }
                Some(bytes)
            });
            let ca_key = ca_key_hex.and_then(|hex| {
                let mut bytes = Vec::new();
                for i in (0..hex.len()).step_by(2) {
                    if let Ok(b) = u8::from_str_radix(&hex[i..i + 2], 16) {
                        bytes.push(b);
                    } else {
                        return None;
                    }
                }
                Some(bytes)
            });

            let security_config =
                squeezefs::tiering::dht::ClusterSecurityConfig { ca_cert, ca_key };

            if let Some(ref addr) = p2p_addr {
                let server = squeezefs::p2p::P2pServer::new(
                    addr.clone(),
                    fs_engine.router.cache.clone(),
                    security_config,
                );
                tokio::spawn(async move {
                    if let Err(e) = server.run().await {
                        log::error!("P2P Server error: {:?}", e);
                    }
                });
            }

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
            squeeze_uri,
            nvme_path,
        } => {
            let (redis_url, name) = resolve_squeeze_uri(squeeze_uri.as_deref(), None)?;
            squeezefs::set_fs_prefix(&name);
            println!("Starting defragmentation for volume '{}'", name);
            squeezefs::defrag::run_defragmentation(&redis_url, &name, &nvme_path).await?;
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
            squeeze_uri,
            src,
            dest,
        } => {
            let (redis_url_str, fs_name) =
                resolve_squeeze_uri(squeeze_uri.as_deref(), Some(std::path::Path::new(&src)))?;
            let redis_url = &redis_url_str;
            squeezefs::set_fs_prefix(&fs_name);
            let staging_dirs = vec![get_default_staging_dir()];

            let dlm = DlmClient::new(redis_url)?;

            // Reconstruct block allocator and nvme block dev for clone operation
            let block_alloc = std::sync::Arc::new(
                squeezefs::block_allocator::BlockAllocator::new(
                    std::sync::Arc::new(dlm.meta_client().clone()),
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

            // Load supplementary backends from Garnet
            if let Ok(mut con) = router.dlm.meta_client().get_connection().await {
                use redis::AsyncCommands;
                let backends_raw: std::collections::HashMap<String, String> = con
                    .hgetall(squeezefs::fs_key!("backends"))
                    .await
                    .unwrap_or_default();
                for (be_id, be_json) in backends_raw {
                    if let Ok(config) = serde_json::from_str::<serde_json::Value>(&be_json) {
                        if let Some(bd) = config["backing_dev"].as_str() {
                            let mut resolved_bd = bd.to_string();
                            if !std::path::Path::new(&resolved_bd).exists() {
                                let ip_val = config["ip"].as_str();
                                let port_val = config["port"].as_u64().map(|p| p as u16);
                                let nqn_val = config["subnqn"].as_str();
                                if let (Some(ip), Some(port), Some(subnqn)) =
                                    (ip_val, port_val, nqn_val)
                                {
                                    log::info!("Supplementary backend '{}' device not found. Connecting to NVMe-oF target at {}:{} / {}...", be_id, ip, port, subnqn);
                                    if let Ok(dev_path) =
                                        squeezefs::nvmeof::connect_target(ip, port, subnqn)
                                    {
                                        log::info!(
                                            "Connected supplementary backend '{}' to: {}",
                                            be_id,
                                            dev_path
                                        );
                                        resolved_bd = dev_path;
                                    }
                                }
                            }
                            let lvm_vg = config["lvm_vg"].as_str();
                            let lvm_loops = config["lvm_loops"]
                                .as_object()
                                .map(|obj| {
                                    obj.iter()
                                        .map(|(k, v)| {
                                            (k.clone(), v.as_str().unwrap_or_default().to_string())
                                        })
                                        .collect::<std::collections::HashMap<String, String>>()
                                })
                                .unwrap_or_default();
                            let _ =
                                squeezefs::storage::restore_lvm_loop_devices(lvm_vg, &lvm_loops);
                            let _resolved_cap = config["capacity"]
                                .as_u64()
                                .unwrap_or(1024 * 1024 * 1024 * 1024);
                            let device = squeezefs::nvme_dev::NvmeBlockDev::new(&resolved_bd);
                            let dev_arc = std::sync::Arc::new(device);
                            let be_alloc_name = format!("{}:{}", fs_name, be_id);
                            if let Ok(allocator) = squeezefs::block_allocator::BlockAllocator::new(
                                std::sync::Arc::new(router.dlm.meta_client().clone()),
                                &be_alloc_name,
                            )
                            .await
                            {
                                router.backend_router.backends.insert(
                                    be_id,
                                    std::sync::Arc::new(squeezefs::routing::StorageBackend {
                                        device: dev_arc,
                                        block_allocator: std::sync::Arc::new(allocator),
                                    }),
                                );
                            }
                        }
                    }
                }

                // Set active write backend
                let active_be: String = con
                    .hget(squeezefs::fs_key!("format"), "active_write_backend")
                    .await
                    .unwrap_or(Some("backend_0".to_string()))
                    .unwrap_or_else(|| "backend_0".to_string());
                router
                    .backend_router
                    .active_write_backend
                    .store(std::sync::Arc::new(active_be));
            }

            println!("Cloning file from {} to {}...", src, dest);
            router.clone_path(&src, &dest).await?;
            println!("File cloned successfully.");
        }
        Commands::Df { squeeze_uri, path } => {
            let ref_path = path.as_ref().map(|p| std::path::Path::new(p));
            let (redis_url, fs_name) = resolve_squeeze_uri(squeeze_uri.as_deref(), ref_path)?;
            squeezefs::set_fs_prefix(&fs_name);
            run_df_command(&redis_url, path).await?;
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
                NvmeofActions::Connect {
                    ip,
                    port,
                    subnqn,
                    local_ips,
                } => {
                    println!("Connecting to NVMe-oF target at {}:{}...", ip, port);
                    let local_ips_vec = local_ips.unwrap_or_default();
                    let dev = squeezefs::nvmeof::connect_target_with_local_ips(
                        &ip,
                        port,
                        &subnqn,
                        &local_ips_vec,
                    )?;
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
            squeeze_uri,
            action,
        } => {
            let (redis_url, fs_name) = resolve_squeeze_uri(squeeze_uri.as_deref(), None)?;
            let garnet_url = &redis_url;
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
                ConfigActions::Volume(action) => match action {
                    VolumeActions::Add {
                        volume_id,
                        volume: backing_dev,
                        ip,
                        port,
                        subnqn,
                        capacity,
                    } => {
                        let backend_id = volume_id;
                        let resolved_capacity = if let Some(ref cap_str) = capacity {
                            if let Ok(bytes) = squeezefs::cache::parse_size_string(cap_str, 0) {
                                Some(bytes)
                            } else {
                                return Err(Box::new(
                                    squeezefs::error::SqueezefsError::InvalidOperation(format!(
                                        "Invalid capacity string: {}",
                                        cap_str
                                    )),
                                ));
                            }
                        } else {
                            None
                        };
                        squeezefs::config_ops::add_storage_backend(
                            &garnet_url,
                            &fs_name,
                            &backend_id,
                            backing_dev.as_deref(),
                            ip.as_deref(),
                            port,
                            subnqn.as_deref(),
                            resolved_capacity,
                        )
                        .await?;
                        println!(
                            "Storage volume '{}' registered/added successfully.",
                            backend_id
                        );
                    }
                    VolumeActions::Remove { volume_id, force } => {
                        let backend_id = volume_id;
                        squeezefs::config_ops::remove_storage_backend(
                            &garnet_url,
                            &fs_name,
                            &backend_id,
                            force,
                        )
                        .await?;
                        println!("Storage volume '{}' removed successfully.", backend_id);
                    }
                    VolumeActions::List => {
                        let list =
                            squeezefs::config_ops::list_config(&garnet_url, &fs_name).await?;
                        println!("{}", serde_json::to_string_pretty(&list.backends)?);
                    }
                    VolumeActions::SetActive { volume_id } => {
                        let backend_id = volume_id;
                        squeezefs::config_ops::set_active_backend(
                            &garnet_url,
                            &fs_name,
                            &backend_id,
                        )
                        .await?;
                        println!("Active write volume set to '{}' successfully.", backend_id);
                    }
                },
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
            squeeze_uri,
            mountpoint,
            force,
        } => {
            let _ctrl_c_guard = spawn_ctrl_c_handler("unmount");
            use std::io::IsTerminal;
            use std::io::Write;

            let (redis_url_str, resolved_fs_name) =
                resolve_squeeze_uri(squeeze_uri.as_deref(), Some(&mountpoint))?;
            let redis_url = &redis_url_str;
            squeezefs::set_fs_prefix(&resolved_fs_name);

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

            // 2. Fall back to Garnet format defaults if config file wasn't readable
            if staging_dirs.is_empty() {
                if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                    if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                        let paths_str: Option<String> = con
                            .hget(squeezefs::fs_key!("format"), "disk_cache_paths")
                            .await
                            .unwrap_or(None);
                        if let Some(s) = paths_str {
                            if !s.is_empty() {
                                staging_dirs = s.split(',').map(PathBuf::from).collect();
                            }
                        }
                    }
                }
            }

            // 3. Fall back to default staging directory
            if staging_dirs.is_empty() {
                staging_dirs = vec![get_default_staging_dir()];
            }

            // 4. Resolve dismount_wait limit (default: 10) and find active daemon PID
            let mut dismount_wait = 10;
            let mut daemon_pid: Option<u32> = None;
            if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                    let wait_str: Option<String> = con
                        .hget(squeezefs::fs_key!("format"), "dismount_wait")
                        .await
                        .unwrap_or(None);
                    if let Some(s) = wait_str {
                        if let Ok(w) = s.parse::<u64>() {
                            dismount_wait = w;
                        }
                    }

                    // Look up active client matching this mountpoint to find daemon PID
                    let raw_clients: std::collections::HashMap<String, String> = con
                        .hgetall(squeezefs::fs_key!("active_clients"))
                        .await
                        .unwrap_or_default();
                    let abs_mountpoint =
                        std::fs::canonicalize(&mountpoint).unwrap_or_else(|_| mountpoint.clone());
                    for (_, json_str) in raw_clients {
                        if let Ok(info) =
                            serde_json::from_str::<squeezefs::fuse_client::ClientInfo>(&json_str)
                        {
                            let client_mount = std::path::Path::new(&info.mountpoint);
                            let abs_client_mount = std::fs::canonicalize(client_mount)
                                .unwrap_or_else(|_| client_mount.to_path_buf());
                            if abs_client_mount == abs_mountpoint {
                                daemon_pid = Some(info.pid);
                                break;
                            }
                        }
                    }
                }
            }

            // 5. Count staged files and active writes from cache segments
            let mut max_write_bytes = 100 * 1024 * 1024;
            if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                    let size_str: Option<String> = con
                        .hget(squeezefs::fs_key!("format"), "write_disk_limit")
                        .await
                        .unwrap_or(None);
                    if let Some(ref s) = size_str {
                        max_write_bytes =
                            squeezefs::cache::parse_size_string(s, 100 * 1024 * 1024 * 1024)
                                .unwrap_or(100 * 1024 * 1024);
                    }
                }
            }

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

                            // 1. Connect to Redis to clear metadata
                            if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                                if let Ok(mut con) = client.get_multiplexed_tokio_connection().await
                                {
                                    for cache in &caches {
                                        for key_bytes in cache.list_keys() {
                                            let is_active = if let Ok(s) =
                                                String::from_utf8(key_bytes.to_vec())
                                            {
                                                s.starts_with("active_block:")
                                            } else {
                                                false
                                            };
                                            if !is_active {
                                                if let Some(guard) = cache.get(&key_bytes) {
                                                    let bytes = &guard.guard.mmap
                                                        [guard.offset..guard.offset + guard.len];
                                                    if bytes.len() >= 8 {
                                                        let meta_len = u64::from_be_bytes(
                                                            bytes[0..8]
                                                                .try_into()
                                                                .unwrap_or([0; 8]),
                                                        )
                                                            as usize;
                                                        if bytes.len() >= 8 + meta_len {
                                                            // Parse metadata to get file_path
                                                            if let Ok(meta) = serde_json::from_slice::<
                                                                serde_json::Value,
                                                            >(
                                                                &bytes[8..8 + meta_len],
                                                            ) {
                                                                if let Some(file_path) = meta
                                                                    .get("file_path")
                                                                    .and_then(|v| v.as_str())
                                                                {
                                                                    let meta_key = format!(
                                                                        "metadata:{}",
                                                                        file_path
                                                                    );
                                                                    let file_id: Option<String> =
                                                                        con.hget(
                                                                            &meta_key, "file_id",
                                                                        )
                                                                        .await
                                                                        .unwrap_or(None);
                                                                    let mut pipe = redis::pipe();
                                                                    pipe.del(&meta_key);
                                                                    if let Some(fid) = file_id {
                                                                        let mapping_key = format!(
                                                                            "mapping:{}",
                                                                            fid
                                                                        );
                                                                        pipe.del(&mapping_key);
                                                                    }
                                                                    let _: () = pipe
                                                                        .query_async(&mut con)
                                                                        .await
                                                                        .unwrap_or(());
                                                                    println!("Removed metadata for unflushed file: {}", file_path);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

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

            // 7. Execute the unmount
            println!("Unmounting squeezefs at {:?}...", mountpoint);
            let status = std::process::Command::new("fusermount")
                .arg("-u")
                .arg(&mountpoint)
                .status();

            let unmount_success = match status {
                Ok(s) if s.success() => true,
                _ => {
                    let umount_status = std::process::Command::new("umount")
                        .arg(&mountpoint)
                        .status();
                    matches!(umount_status, Ok(s) if s.success())
                }
            };

            if unmount_success {
                println!("Successfully unmounted mountpoint {:?}", mountpoint);
                if let Some(pid) = daemon_pid {
                    let proc_path = format!("/proc/{}", pid);
                    if std::path::Path::new(&proc_path).exists() {
                        print!("Waiting for background FUSE daemon process (PID {}) to flush data and exit...", pid);
                        let _ = std::io::stdout().flush();
                        let start_wait = std::time::Instant::now();
                        let max_wait = std::time::Duration::from_secs(dismount_wait);
                        while std::path::Path::new(&proc_path).exists() {
                            if start_wait.elapsed() >= max_wait {
                                println!("\nWarning: Background daemon process (PID {}) did not exit within {} seconds.", pid, dismount_wait);
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                        if !std::path::Path::new(&proc_path).exists() {
                            println!(" done.");
                        }
                    }
                }
            } else {
                eprintln!(
                    "Error: Failed to unmount mountpoint {:?}. Try running with sudo.",
                    mountpoint
                );
                std::process::exit(1);
            }
        }
    }

    Ok(())
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

fn resolve_fs_name_from_mount(mnt: &std::path::Path) -> Option<String> {
    let config_path = mnt.join(".config");
    if let Ok(config_str) = std::fs::read_to_string(&config_path) {
        if let Ok(config_json) = serde_json::from_str::<serde_json::Value>(&config_str) {
            if let Some(n) = config_json["format"]["name"].as_str() {
                return Some(n.to_string());
            }
        }
    }
    if let Some(uri) = find_uri_from_proc(mnt) {
        if let Ok((_, fs_name)) = parse_squeeze_uri(&uri) {
            return Some(fs_name);
        }
    }
    None
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

async fn resolve_path_to_inode(
    dlm: &DlmClient,
    path: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut current_ino = 1u64; // Root inode
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        let mut con = dlm.get_connection_for_inode(current_ino).await?;
        let dir_key = format!("{}:dir:{}", squeezefs::fs_prefix(), current_ino);
        let next_ino_opt: Option<u64> = con.hget(&dir_key, part).await?;
        match next_ino_opt {
            Some(next_ino) => {
                current_ino = next_ino;
            }
            None => {
                return Err(format!(
                    "Path component '{}' not found in inode {}",
                    part, current_ino
                )
                .into());
            }
        }
    }
    Ok(current_ino)
}

async fn run_df_command(
    redis_url: &str,
    path_opt: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mounts = find_squeezefs_mounts();
    let (path_opt, target_mount) = match path_opt {
        Some(ref p) if is_uri(p) => (None, None),
        Some(ref p) => {
            let abs_path = std::path::Path::new(p);
            let norm_p = abs_path.to_string_lossy().trim_end_matches('/').to_string();

            let matched_mnt = mounts.iter().find(|mnt| {
                let norm_mnt = mnt.to_string_lossy().trim_end_matches('/').to_string();
                norm_p == norm_mnt
            });

            if let Some(mnt) = matched_mnt {
                (None, Some(mnt.clone()))
            } else {
                let containing_mnt = mounts.iter().find(|mnt| {
                    let norm_mnt = mnt.to_string_lossy().trim_end_matches('/').to_string();
                    norm_p.starts_with(&format!("{}/", norm_mnt))
                });
                if containing_mnt.is_some() {
                    (Some(p.clone()), None)
                } else {
                    let mut matching_mount = None;
                    let mut current = std::path::PathBuf::from(p);
                    loop {
                        if current.join(".stats").exists() && current.join(".config").exists() {
                            matching_mount = Some(current.clone());
                            break;
                        }
                        if !current.pop() {
                            break;
                        }
                    }
                    let is_mount_query = if let Some(ref mnt) = matching_mount {
                        let norm_mnt = mnt.to_string_lossy().trim_end_matches('/').to_string();
                        norm_p == norm_mnt
                    } else {
                        false
                    };
                    if is_mount_query {
                        (None, matching_mount)
                    } else {
                        (Some(p.clone()), None)
                    }
                }
            }
        }
        None => (None, None),
    };
    let dlm = DlmClient::new(redis_url)?;
    let mut con = dlm.get_connection().await?;

    match path_opt {
        None => {
            let mut resolved_fs_name = "squeezefs".to_string();
            let mounts_to_use = if let Some(ref mnt) = target_mount {
                vec![mnt.clone()]
            } else {
                mounts.clone()
            };
            if !mounts_to_use.is_empty() {
                if let Some(name) = resolve_fs_name_from_mount(&mounts_to_use[0]) {
                    resolved_fs_name = name;
                }
            }
            squeezefs::set_fs_prefix(&resolved_fs_name);

            let format_fields: HashMap<String, String> = con
                .hgetall(squeezefs::fs_key!("format"))
                .await
                .unwrap_or_default();
            let capacity_bytes: u64 = format_fields
                .get("capacity")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let capacity_str = if capacity_bytes == 0 {
                "Unlimited".to_string()
            } else {
                format_size(capacity_bytes)
            };

            let keys: Vec<String> = redis::cmd("KEYS")
                .arg("metadata:*")
                .query_async(&mut con)
                .await
                .unwrap_or_default();

            let mut total_logical_size = 0u64;
            if !keys.is_empty() {
                let mut pipe = redis::pipe();
                for k in &keys {
                    pipe.hget(k, "size");
                }
                let results: Vec<Option<String>> =
                    pipe.query_async(&mut con).await.unwrap_or_default();
                for s in results.into_iter().flatten() {
                    let size: u64 = s.parse().unwrap_or(0);
                    total_logical_size += size;
                }
            }

            let block_sizes: HashMap<String, String> = con
                .hgetall(squeezefs::fs_key!("block_sizes"))
                .await
                .unwrap_or_default();
            let mut total_physical_size = 0u64;
            for val in block_sizes.values() {
                let parts: Vec<&str> = val.split(':').collect();
                if parts.len() == 2 {
                    let physical: u64 = parts[1].parse().unwrap_or(0);
                    total_physical_size += physical;
                }
            }

            let ratio_str = if total_physical_size > 0 {
                format!(
                    "{:.2}x",
                    total_logical_size as f64 / total_physical_size as f64
                )
            } else {
                "1.00x".to_string()
            };

            println!(
                "{}",
                "SqueezeFS Filesystem Space Usage Summary:".bold().cyan()
            );
            println!("--------------------------------------------------");
            println!("Capacity:            {}", capacity_str);
            println!("Logical File Size:   {}", format_size(total_logical_size));
            println!(
                "Physical Backend Size: {}",
                format_size(total_physical_size)
            );
            println!("Compression Ratio:   {}", ratio_str);
            println!();

            if mounts.is_empty() {
                println!("No active SqueezeFS mounts detected.");
            } else {
                println!("{}", "Active Mounts Cache Usage:".bold().cyan());
                println!(
                    "{:<20} {:<20} {:<20} {:<20} {:<20}",
                    "Mountpoint",
                    "RAM Read Cache",
                    "RAM Write Cache",
                    "NVMe Read Cache",
                    "NVMe Staging"
                );
                println!("{}", "-".repeat(100));
                for mnt in &mounts {
                    let stats_path = mnt.join(".stats");
                    match std::fs::read_to_string(&stats_path) {
                        Ok(stats_str) => {
                            if let Ok(stats_json) =
                                serde_json::from_str::<serde_json::Value>(&stats_str)
                            {
                                let cap = &stats_json["cache_capacities"];
                                let ram_read_curr =
                                    cap["read_lru_current_bytes"].as_u64().unwrap_or(0);
                                let ram_read_max = cap["read_lru_max_bytes"].as_u64().unwrap_or(0);
                                let ram_write_curr =
                                    cap["write_lru_current_bytes"].as_u64().unwrap_or(0);
                                let ram_write_max =
                                    cap["write_lru_max_bytes"].as_u64().unwrap_or(0);
                                let nvme_read_curr =
                                    cap["nvme_read_cache_current_bytes"].as_u64().unwrap_or(0);
                                let nvme_read_max =
                                    cap["nvme_read_cache_max_bytes"].as_u64().unwrap_or(0);
                                let nvme_stage_curr =
                                    cap["nvme_staging_current_bytes"].as_u64().unwrap_or(0);
                                let nvme_stage_max =
                                    cap["nvme_staging_max_bytes"].as_u64().unwrap_or(0);

                                println!(
                                    "{:<20} {:<20} {:<20} {:<20} {:<20}",
                                    mnt.to_string_lossy(),
                                    format!(
                                        "{} / {}",
                                        format_size(ram_read_curr),
                                        format_size(ram_read_max)
                                    ),
                                    format!(
                                        "{} / {}",
                                        format_size(ram_write_curr),
                                        format_size(ram_write_max)
                                    ),
                                    format!(
                                        "{} / {}",
                                        format_size(nvme_read_curr),
                                        format_size(nvme_read_max)
                                    ),
                                    format!(
                                        "{} / {}",
                                        format_size(nvme_stage_curr),
                                        format_size(nvme_stage_max)
                                    )
                                );
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                            println!(
                                "{:<20} [Permission Denied - run without sudo to view cache stats]",
                                mnt.to_string_lossy()
                            );
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        Some(path_str) => {
            let abs_path = std::path::Path::new(&path_str)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(&path_str));

            let norm_p = abs_path.to_string_lossy().trim_end_matches('/').to_string();
            let mut matching_mount = mounts
                .iter()
                .find(|mnt| {
                    let norm_mnt = mnt.to_string_lossy().trim_end_matches('/').to_string();
                    norm_p.starts_with(&format!("{}/", norm_mnt))
                })
                .cloned();

            if matching_mount.is_none() {
                let mut current = abs_path.clone();
                loop {
                    if current.join(".stats").exists() && current.join(".config").exists() {
                        matching_mount = Some(current.clone());
                        break;
                    }
                    if !current.pop() {
                        break;
                    }
                }
            }

            let mut resolved_fs_name = "squeezefs".to_string();
            if let Some(ref mnt) = matching_mount {
                if let Some(name) = resolve_fs_name_from_mount(mnt) {
                    resolved_fs_name = name;
                }
            }
            squeezefs::set_fs_prefix(&resolved_fs_name);

            let (relative_path_str, stats_json) = if let Some(ref mnt) = matching_mount {
                let rel = abs_path.strip_prefix(mnt).unwrap_or(&abs_path);
                let rel_str = format!("/{}", rel.to_string_lossy().trim_start_matches('/'));
                let stats_path = mnt.join(".stats");
                let json_val = std::fs::read_to_string(&stats_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                (rel_str, json_val)
            } else {
                let rel_str = format!("/{}", abs_path.to_string_lossy().trim_start_matches('/'));
                (rel_str, None)
            };

            let mut ram_keys = std::collections::HashSet::new();
            let mut nvme_keys = std::collections::HashSet::new();
            let mut nvme_staged_file_ids = std::collections::HashSet::new();

            if let Some(ref json) = stats_json {
                if let Some(arr) = json["read_lru_keys"].as_array() {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            ram_keys.insert(s.to_string());
                        }
                    }
                }
                if let Some(arr) = json["write_lru_keys"].as_array() {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            ram_keys.insert(s.to_string());
                        }
                    }
                }
                if let Some(arr) = json["nvme_read_cache_block_keys"].as_array() {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            nvme_keys.insert(s.to_string());
                        }
                    }
                }
                if let Some(arr) = json["nvme_staged_write_file_ids"].as_array() {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            nvme_staged_file_ids.insert(s.to_string());
                        }
                    }
                }
            }

            let mut resolved_meta_key = None;
            let mut resolved_ino = None;

            if let Ok(inode) = resolve_path_to_inode(&dlm, &relative_path_str).await {
                resolved_meta_key = Some(format!("metadata:inode_{}", inode));
                resolved_ino = Some(inode);
            } else {
                let path_stripped = relative_path_str.trim_start_matches('/').to_string();
                let keys_to_try = vec![
                    format!("metadata:{}", relative_path_str),
                    format!("metadata:{}", path_stripped),
                ];
                for k in keys_to_try {
                    let exists: bool = con.exists(&k).await.unwrap_or(false);
                    if exists {
                        resolved_meta_key = Some(k);
                        break;
                    }
                }
            }

            let meta_key = match resolved_meta_key {
                Some(k) => k,
                None => {
                    eprintln!("Error: Path '{}' not found in metadata", path_str);
                    std::process::exit(1);
                }
            };

            let kind: Option<String> = con.hget(&meta_key, "type").await?;
            let size: Option<u64> = con.hget(&meta_key, "size").await?;

            let kind_str = kind.unwrap_or_else(|| "striped".to_string());
            let size_bytes = size.unwrap_or(0);

            println!(
                "{}",
                format!("File Space Usage Details for: {}", path_str)
                    .bold()
                    .cyan()
            );
            println!("--------------------------------------------------");
            if let Some(ino) = resolved_ino {
                println!("Inode:        {}", ino);
            } else {
                println!("Inode:        N/A (Direct Path Layout)");
            }
            println!("Logical Size: {}", format_size(size_bytes));
            println!("Layout Type:  {}", kind_str);
            println!();

            if kind_str == "inline" {
                let path_stripped = relative_path_str.trim_start_matches('/').to_string();
                let inline_key = if let Some(ino) = resolved_ino {
                    format!("inline_data:inode_{}", ino)
                } else {
                    let actual_path = meta_key.strip_prefix("metadata:").unwrap_or(&path_stripped);
                    format!("inline_data:{}", actual_path)
                };
                let inline_len: u64 = redis::cmd("STRLEN")
                    .arg(&inline_key)
                    .query_async(&mut con)
                    .await
                    .unwrap_or(0);
                let ratio_str = if inline_len > 0 {
                    format!("{:.2}x", size_bytes as f64 / inline_len as f64)
                } else {
                    "1.00x".to_string()
                };

                println!(
                    "{:<6} {:<40} {:<15} {:<15} {:<8} {:<15}",
                    "Block",
                    "Key / Location",
                    "Logical Size",
                    "Physical Size",
                    "Ratio",
                    "Residency"
                );
                println!("{}", "-".repeat(100));
                println!(
                    "{:<6} {:<40} {:<15} {:<15} {:<8} {:<15}",
                    0,
                    inline_key,
                    format_size(size_bytes),
                    format_size(inline_len),
                    ratio_str,
                    "RAM, DB"
                );
            } else if kind_str == "staged" {
                let file_id: Option<String> = con.hget(&meta_key, "file_id").await?;
                if let Some(fid) = file_id {
                    let is_staged_in_nvme = nvme_staged_file_ids.contains(&fid);
                    if is_staged_in_nvme {
                        println!(
                            "{:<6} {:<40} {:<15} {:<15} {:<8} {:<15}",
                            "Block",
                            "Key / Location",
                            "Logical Size",
                            "Physical Size",
                            "Ratio",
                            "Residency"
                        );
                        println!("{}", "-".repeat(100));
                        println!(
                            "{:<6} {:<40} {:<15} {:<15} {:<8} {:<15}",
                            0,
                            format!("staging_file:{}", fid),
                            format_size(size_bytes),
                            format_size(size_bytes),
                            "1.00x",
                            "NVMe Staging"
                        );
                    } else {
                        let mapping_key = format!("mapping:{}", fid);
                        let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                        let offset_opt: Option<u64> = con.hget(&mapping_key, "offset").await?;
                        let offset = offset_opt.unwrap_or(0);
                        let physical_size_opt: Option<u64> = con.hget(&mapping_key, "size").await?;
                        let physical_size = physical_size_opt.unwrap_or(size_bytes);

                        let bk = block_key.unwrap_or_else(|| "Unknown".to_string());
                        let ratio_str = if physical_size > 0 {
                            format!("{:.2}x", size_bytes as f64 / physical_size as f64)
                        } else {
                            "1.00x".to_string()
                        };

                        let mut locations = Vec::new();
                        if ram_keys.contains(&bk) {
                            locations.push("RAM");
                        }
                        if nvme_keys.contains(&bk) {
                            locations.push("NVMe");
                        }
                        if locations.is_empty() {
                            locations.push("NVMe-oF Backend");
                        }
                        let residency = locations.join(", ");

                        println!(
                            "{:<6} {:<40} {:<15} {:<15} {:<8} {:<15}",
                            "Block",
                            "Key / Location",
                            "Logical Size",
                            "Physical Size",
                            "Ratio",
                            "Residency"
                        );
                        println!("{}", "-".repeat(100));
                        println!(
                            "{:<6} {:<40} {:<15} {:<15} {:<8} {:<15}",
                            0,
                            format!("{} (offset {})", bk, offset),
                            format_size(size_bytes),
                            format_size(physical_size),
                            ratio_str,
                            residency
                        );
                    }
                } else {
                    println!("No staging mapping found for file.");
                }
            } else {
                let block_map_id: Option<String> = con.hget(&meta_key, "block_map_id").await?;
                if let Some(map_id) = block_map_id {
                    let block_map_key = format!("{}:block_map:{}", squeezefs::fs_prefix(), map_id);
                    let block_map: HashMap<String, String> =
                        con.hgetall(&block_map_key).await.unwrap_or_default();

                    let mut indices: Vec<u32> = block_map
                        .keys()
                        .filter_map(|k| k.parse::<u32>().ok())
                        .collect();
                    indices.sort_unstable();

                    println!(
                        "{:<6} {:<40} {:<15} {:<15} {:<8} {:<15}",
                        "Block", "Block Key", "Logical Size", "Physical Size", "Ratio", "Residency"
                    );
                    println!("{}", "-".repeat(100));

                    for idx in indices {
                        if let Some(bk) = block_map.get(&idx.to_string()) {
                            let size_info: Option<String> =
                                con.hget(squeezefs::fs_key!("block_sizes"), bk).await?;
                            let (log_sz, phys_sz) = if let Some(info) = size_info {
                                let parts: Vec<&str> = info.split(':').collect();
                                if parts.len() == 2 {
                                    (
                                        parts[0].parse().unwrap_or(0u64),
                                        parts[1].parse().unwrap_or(0u64),
                                    )
                                } else {
                                    (0u64, 0u64)
                                }
                            } else {
                                (0u64, 0u64)
                            };

                            let ratio_str = if phys_sz > 0 {
                                format!("{:.2}x", log_sz as f64 / phys_sz as f64)
                            } else {
                                "1.00x".to_string()
                            };

                            let mut locations = Vec::new();
                            if ram_keys.contains(bk) {
                                locations.push("RAM");
                            }
                            if nvme_keys.contains(bk) {
                                locations.push("NVMe");
                            }
                            if locations.is_empty() {
                                locations.push("NVMe-oF Backend");
                            }
                            let residency = locations.join(", ");

                            println!(
                                "{:<6} {:<40} {:<15} {:<15} {:<8} {:<15}",
                                idx,
                                bk,
                                if log_sz > 0 {
                                    format_size(log_sz)
                                } else {
                                    "Unknown".to_string()
                                },
                                if phys_sz > 0 {
                                    format_size(phys_sz)
                                } else {
                                    "Unknown".to_string()
                                },
                                ratio_str,
                                residency
                            );
                        }
                    }
                } else {
                    println!("No block map found for file.");
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_sqpoll_idle_prefers_explicit_override() {
        let mut format_fields = HashMap::new();
        format_fields.insert(
            FUSE_IO_URING_SQPOLL_IDLE_MS_KEY.to_string(),
            "250".to_string(),
        );

        assert_eq!(
            resolve_shared_sqpoll_idle_ms(Some(100), &format_fields).unwrap(),
            Some(100)
        );
    }

    #[test]
    fn resolve_sqpoll_idle_allows_explicit_disable() {
        let mut format_fields = HashMap::new();
        format_fields.insert(
            FUSE_IO_URING_SQPOLL_IDLE_MS_KEY.to_string(),
            "250".to_string(),
        );

        assert_eq!(
            resolve_shared_sqpoll_idle_ms(Some(0), &format_fields).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_sqpoll_idle_uses_shared_metadata_default() {
        let mut format_fields = HashMap::new();
        format_fields.insert(
            FUSE_IO_URING_SQPOLL_IDLE_MS_KEY.to_string(),
            "300".to_string(),
        );

        assert_eq!(
            resolve_shared_sqpoll_idle_ms(None, &format_fields).unwrap(),
            Some(300)
        );
    }

    #[test]
    fn test_parse_squeeze_uri_valid() {
        let (redis, name) = parse_squeeze_uri("squeeze://127.0.0.1:6379/myvolume").unwrap();
        assert_eq!(redis, "redis://127.0.0.1:6379");
        assert_eq!(name, "myvolume");
    }

    #[test]
    fn test_parse_squeeze_uri_redis_fallback() {
        let (redis, name) = parse_squeeze_uri("redis://127.0.0.1:6379").unwrap();
        assert_eq!(redis, "redis://127.0.0.1:6379");
        assert_eq!(name, "squeezefs");
    }

    #[test]
    fn test_parse_squeeze_uri_invalid() {
        assert!(parse_squeeze_uri("http://127.0.0.1").is_err());
        assert!(parse_squeeze_uri("squeeze://127.0.0.1").is_err());
        assert!(parse_squeeze_uri("squeeze://127.0.0.1/").is_err());
    }

    #[test]
    fn test_resolve_squeeze_uri_direct_uri() {
        let (redis, name) =
            resolve_squeeze_uri(Some("squeeze://127.0.0.1:6379/vol"), None).unwrap();
        assert_eq!(redis, "redis://127.0.0.1:6379");
        assert_eq!(name, "vol");

        let (redis, name) = resolve_squeeze_uri(
            None,
            Some(std::path::Path::new("squeeze://127.0.0.1:6379/vol")),
        )
        .unwrap();
        assert_eq!(redis, "redis://127.0.0.1:6379");
        assert_eq!(name, "vol");
    }
}

fn is_uri(s: &str) -> bool {
    s.starts_with("squeeze://") || s.starts_with("redis://") || s.starts_with("redis+cluster://")
}

fn parse_squeeze_uri(uri: &str) -> Result<(String, String), String> {
    if uri.starts_with("squeeze://") {
        let rest = &uri["squeeze://".len()..];
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(format!(
                "Invalid SqueezeFS URI format: expected 'squeeze://host:port/fs_name' (got '{}')",
                uri
            ));
        }
        let redis_url = format!("redis://{}", parts[0]);
        let fs_name = parts[1].to_string();
        Ok((redis_url, fs_name))
    } else if uri.starts_with("redis://") {
        Ok((uri.to_string(), "squeezefs".to_string()))
    } else {
        Err(format!(
            "Invalid SqueezeFS URI scheme: expected 'squeeze://' (got '{}')",
            uri
        ))
    }
}

fn resolve_squeeze_uri(
    cli_uri: Option<&str>,
    reference_path: Option<&std::path::Path>,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    if let Some(uri) = cli_uri {
        if is_uri(uri) {
            return Ok(parse_squeeze_uri(uri)?);
        } else {
            if !uri.contains('/') && !uri.contains('\\') {
                if let Ok(garnet_url) = std::env::var("GARNET_URL") {
                    let host_port = garnet_url
                        .trim_start_matches("redis://")
                        .trim_start_matches("redis+cluster://");
                    let squeeze_format = format!("squeeze://{}/{}", host_port, uri);
                    if let Ok(res) = parse_squeeze_uri(&squeeze_format) {
                        return Ok(res);
                    }
                }
            }
            let path = std::path::Path::new(uri);
            return resolve_squeeze_uri(None, Some(path));
        }
    }
    if let Some(ref_path) = reference_path {
        if let Some(path_str) = ref_path.to_str() {
            if is_uri(path_str) {
                return Ok(parse_squeeze_uri(path_str)?);
            }
        }
        if let Some(uri) = find_uri_from_proc(ref_path) {
            return Ok(parse_squeeze_uri(&uri)?);
        }
        let mut current = if ref_path.is_file() {
            ref_path.parent().unwrap_or(ref_path).to_path_buf()
        } else {
            ref_path.to_path_buf()
        };
        loop {
            let config_path = current.join(".config");
            if let Ok(config_str) = std::fs::read_to_string(&config_path) {
                if let Ok(config_json) = serde_json::from_str::<serde_json::Value>(&config_str) {
                    if let Some(url_str) = config_json.get("garnet_url").and_then(|v| v.as_str()) {
                        let name_str = config_json["format"]["name"]
                            .as_str()
                            .unwrap_or("squeezefs")
                            .to_string();
                        return Ok((url_str.to_string(), name_str));
                    }
                }
            }
            if let Some(parent) = current.parent() {
                current = parent.to_path_buf();
            } else {
                break;
            }
        }
    }
    let mounts = find_squeezefs_mounts();
    for mount in mounts {
        let config_path = mount.join(".config");
        if let Ok(config_str) = std::fs::read_to_string(&config_path) {
            if let Ok(config_json) = serde_json::from_str::<serde_json::Value>(&config_str) {
                if let Some(url_str) = config_json.get("garnet_url").and_then(|v| v.as_str()) {
                    let name_str = config_json["format"]["name"]
                        .as_str()
                        .unwrap_or("squeezefs")
                        .to_string();
                    return Ok((url_str.to_string(), name_str));
                }
            }
        } else if let Some(uri) = find_uri_from_proc(&mount) {
            if let Ok(res) = parse_squeeze_uri(&uri) {
                return Ok(res);
            }
        }
    }
    if let Ok(uri) = std::env::var("GARNET_URL") {
        return Ok(parse_squeeze_uri(&uri)?);
    }
    if let Ok(uri) = std::env::var("SQUEEZE_URI") {
        return Ok(parse_squeeze_uri(&uri)?);
    }
    Err("Error: SqueezeFS URI must be specified via parameter, environment variable, or resolved from a squeezefs mountpoint".into())
}

async fn get_daemon_metrics(redis_url: &str) -> Option<HashMap<String, u64>> {
    let client = squeezefs::dlm::MetaClient::new(redis_url).ok()?;
    let mut con = client.get_connection().await.ok()?;
    let metrics: HashMap<String, String> = con
        .hgetall(squeezefs::fs_key!("metrics:daemon"))
        .await
        .ok()?;

    let mut parsed = HashMap::new();
    for (k, v) in metrics {
        if let Ok(val) = v.parse::<u64>() {
            parsed.insert(k, val);
        }
    }
    Some(parsed)
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
