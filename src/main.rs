#![allow(clippy::items_after_test_module)]

use clap::{Parser, Subcommand};
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use redis::AsyncCommands;
use squeezefs::backend::{MultiBackendClient, RustFsClient};
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

    #[arg(
        long,
        short = 'g',
        global = true,
        env = "GARNET_URL",
        default_value = "redis://127.0.0.1:6379"
    )]
    garnet_url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Format Garnet database to initialize squeezefs volume
    Format {
        /// Volume name
        name: String,
        /// Block size (e.g. "4M", "1M", default: 4MB)
        #[arg(long, default_value = "4M")]
        block_size: String,
        /// Maximum capacity of the volume (e.g. "1P", "100G", default: 1PB)
        #[arg(long, default_value = "1P")]
        capacity: String,
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
        /// S3 compatible object store endpoint url
        #[arg(long, alias = "endpoint")]
        s3_endpoint: Option<String>,
        /// S3 compatible object store access key
        #[arg(long, alias = "access-key")]
        s3_access_key: Option<String>,
        /// S3 compatible object store secret key
        #[arg(long, alias = "secret-key")]
        s3_secret_key: Option<String>,
        /// S3 compatible object store bucket name
        #[arg(long, alias = "bucket")]
        s3_bucket: Option<String>,
        /// Force formatting even if a squeezefs volume is already detected
        #[arg(long, short = 'f')]
        force: bool,
        /// Perform quick format (initialize metadata only, do not wipe object storage buckets)
        #[arg(long)]
        quick: bool,
        /// Compression algorithm (lz4, zstd, none, default: none)
        #[arg(long, default_value = "none")]
        compression: String,
        /// Encryption algorithm (aes256gcm-rsa, chacha20-rsa, none, default: none)
        #[arg(long, default_value = "none")]
        encrypt_algo: String,
        /// Path to RSA private key PEM file for client-side encryption
        #[arg(long)]
        encrypt_key: Option<String>,
        /// Time in seconds to wait for staged writes to drain to S3 on dismount (default: 10)
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
    Status,
    /// Mount squeezefs at a target path
    Mount {
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

        /// S3 compatible object store endpoint url (overrides stored configuration)
        #[arg(long, alias = "endpoint")]
        s3_endpoint: Option<String>,
        /// S3 compatible object store access key (overrides stored configuration)
        #[arg(long, alias = "access-key")]
        s3_access_key: Option<String>,
        /// S3 compatible object store secret key (overrides stored configuration)
        #[arg(long, alias = "secret-key")]
        s3_secret_key: Option<String>,
        /// S3 compatible object store bucket name (overrides stored configuration)
        #[arg(long, alias = "bucket")]
        s3_bucket: Option<String>,

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

        /// Max concurrent background uploads to S3 (default: 16)
        #[arg(long, default_value_t = 16)]
        max_background_uploads: usize,

        /// Allow other users to access the mount
        #[arg(long)]
        allow_other: bool,

        /// Validate backend storage connectivity on startup
        #[arg(long)]
        check_storage: bool,

        /// Time in seconds to wait for staged writes to drain to S3 on dismount (default: 10)
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
    },
    /// Cleanly unmount a squeezefs mountpoint, with options to cancel, wait, or force dismount
    Umount {
        /// Path to the mountpoint
        mountpoint: PathBuf,
        /// Force unmount immediately without prompting/waiting
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Benchmark performance of the filesystem
    Bench {
        /// Path to the mounted filesystem directory
        path: PathBuf,
        /// Number of concurrent threads
        #[arg(short, long, default_value_t = 1)]
        threads: usize,
        /// Size of the big file in MB per thread
        #[arg(long, default_value_t = 128)]
        size: usize,
        /// Number of iterations to run the benchmark
        #[arg(short, long, default_value_t = 1)]
        iterations: usize,
    },
    /// Clone a file metadata-only (instant Copy-on-Write cloning)
    Clone {
        /// Source file path
        src: String,
        /// Destination file path
        dest: String,
    },
    /// Automatically tune client node configurations (requires root/sudo to apply changes)
    Tune,
    /// Configuration management utility
    Config {
        /// Garnet/Redis URL
        garnet_url: String,
        /// Volume name (filesystem name)
        fs_name: String,
        #[command(subcommand)]
        action: ConfigActions,
    },
    /// Show filesystem disk space usage across all caches and S3
    Df {
        /// Optional path to a file or directory
        path: Option<String>,
    },
    /// NVMe over Fabrics configuration and management utility
    Nvmeof {
        #[command(subcommand)]
        action: NvmeofActions,
    },
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
        /// IP address to bind target to (default: 0.0.0.0)
        #[arg(long, default_value = "0.0.0.0")]
        ip: String,
    },
    /// Stop sharing an NVMe-oF target subsystem
    Unshare {
        /// Subsystem NQN to unshare
        subnqn: String,
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
}

#[derive(Subcommand, Debug, Clone)]
enum ConfigActions {
    /// Manage storage backends
    #[command(subcommand, alias = "backends")]
    Backend(BackendActions),
    /// Manage staging disk caches
    #[command(subcommand, alias = "diskcaches")]
    DiskCache(DiskCacheActions),
    /// Set runtime configuration quotas (capacity, inodes, or memory cache sizes)
    Set {
        /// Quota/config key (e.g. "capacity", "inodes", "mem_cache_size", "read_mem_cache_size", "write_mem_cache_size", "fuse_io_uring_sqpoll_idle_ms")
        key: String,
        /// New value (e.g. "100G", "2T" or numeric value/0)
        value: String,
    },
    /// List current configuration (diskcaches, backends, active backend)
    List,
    /// Consistency check on metadata and block references
    Fsck,
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

#[derive(Subcommand, Debug, Clone)]
enum BackendActions {
    /// Add a storage backend
    Add {
        /// Name/ID of the backend (e.g. backend_1, my-rustfs)
        name: String,
        /// S3 compatible object store endpoint url
        #[arg(long, alias = "endpoint")]
        s3_endpoint: String,
        /// S3 compatible object store access key
        #[arg(long, alias = "access-key", default_value = "admin")]
        s3_access_key: String,
        /// S3 compatible object store secret key
        #[arg(long, alias = "secret-key", default_value = "password")]
        s3_secret_key: String,
        /// S3 compatible object store bucket name
        #[arg(long, alias = "bucket", default_value = "squeezefs-data")]
        s3_bucket: String,
    },
    /// Remove a storage backend
    Remove {
        /// Name of the backend
        name: String,
        /// Force removal ignoring safety checks
        #[arg(long)]
        force: bool,
    },
    /// Enable a storage backend for writes
    Enable {
        /// Name of the backend
        name: String,
    },
    /// Disable a storage backend (no new writes, but existing blocks are still readable)
    Disable {
        /// Name of the backend
        name: String,
    },
    /// List all storage backends and their status
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
                | Commands::Status => {
                    eprintln!("Error: This command must be run as root (or with sudo).");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
    }

    #[cfg(unix)]
    if let Commands::Mount {
        mountpoint,
        mem_cache_size,
        disk_cache_size,
        disk_cache_paths,
        s3_endpoint,
        s3_access_key,
        s3_secret_key,
        s3_bucket,
        daemon,
        no_writeback,
        max_background_uploads,
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
            &cli.garnet_url,
            mountpoint,
            *daemon,
            mem_cache_size.as_deref(),
            disk_cache_size.as_deref(),
            read_cache_size.as_deref(),
            write_cache_size.as_deref(),
            read_mem_cache_size.as_deref(),
            write_mem_cache_size.as_deref(),
            disk_cache_paths.as_deref(),
            s3_endpoint.as_deref(),
            s3_access_key.as_deref(),
            s3_secret_key.as_deref(),
            s3_bucket.as_deref(),
            writeback,
            *max_background_uploads,
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

    let rt = tokio::runtime::Builder::new_multi_thread()
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
    daemon: bool,
    mem_cache_size: Option<&str>,
    disk_cache_size: Option<&str>,
    read_cache_size: Option<&str>,
    write_cache_size: Option<&str>,
    read_mem_cache_size: Option<&str>,
    write_mem_cache_size: Option<&str>,
    disk_cache_paths: Option<&[PathBuf]>,
    s3_endpoint: Option<&str>,
    s3_access_key: Option<&str>,
    s3_secret_key: Option<&str>,
    s3_bucket: Option<&str>,
    writeback: bool,
    max_background_uploads: usize,
    allow_other: bool,
    options: Option<&str>,
    fuse_io_uring_sqpoll_idle_ms: Option<u32>,
    fuse_io_uring_sqpoll_cpu: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    use redis::Commands;
    let client = redis::Client::open(garnet_url)?;
    let mut con = client.get_connection()?;
    let format_fields: std::collections::HashMap<String, String> =
        con.hgetall("squeezefs:format")?;
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
        } else {
            dirs.to_vec()
        }
    } else if let Some(paths_str) = format_fields.get("disk_cache_paths") {
        if paths_str.is_empty() {
            vec![get_default_staging_dir()]
        } else {
            paths_str.split(',').map(PathBuf::from).collect()
        }
    } else {
        vec![get_default_staging_dir()]
    };

    let final_s3_endpoint = s3_endpoint
        .map(|s| s.to_string())
        .or_else(|| std::env::var("RUSTFS_ENDPOINT").ok())
        .or_else(|| std::env::var("AWS_ENDPOINT_URL").ok())
        .or_else(|| {
            format_fields
                .get("s3_endpoint")
                .filter(|s| !s.is_empty())
                .cloned()
        });
    let final_s3_access_key = s3_access_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("RUSTFS_ACCESS_KEY").ok())
        .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok())
        .or_else(|| {
            format_fields
                .get("s3_access_key")
                .filter(|s| !s.is_empty())
                .cloned()
        });
    let final_s3_secret_key = s3_secret_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("RUSTFS_SECRET_KEY").ok())
        .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok())
        .or_else(|| {
            format_fields
                .get("s3_secret_key")
                .filter(|s| !s.is_empty())
                .cloned()
        });
    let final_s3_bucket = s3_bucket
        .map(|s| s.to_string())
        .or_else(|| std::env::var("RUSTFS_BUCKET").ok())
        .or_else(|| {
            format_fields
                .get("s3_bucket")
                .filter(|s| !s.is_empty())
                .cloned()
        });

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

    let masked_access_key = final_s3_access_key
        .as_ref()
        .map(|s| {
            if s.is_empty() {
                "none".to_string()
            } else {
                "******".to_string()
            }
        })
        .unwrap_or_else(|| "none".to_string());
    let masked_secret_key = final_s3_secret_key
        .as_ref()
        .map(|s| {
            if s.is_empty() {
                "none".to_string()
            } else {
                "******".to_string()
            }
        })
        .unwrap_or_else(|| "none".to_string());
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
    println!("  Max Background Uploads: {}", max_background_uploads);
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
    println!(
        "  S3 Endpoint: {:?}",
        final_s3_endpoint.as_deref().unwrap_or("")
    );
    println!("  S3 Access Key: {}", masked_access_key);
    println!("  S3 Secret Key: {}", masked_secret_key);
    println!(
        "  S3 Bucket: {:?}",
        final_s3_bucket.as_deref().unwrap_or("squeezefs-data")
    );
    println!("Security & Compression:");
    println!("  Compression: {:?}", compression);
    println!("  Encryption Algorithm: {:?}", encrypt_algo);
    println!("  Encryption Key: {}", masked_encrypt_key);
    println!("===================================================");

    Ok(())
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

async fn test_storage(client: &RustFsClient) -> Result<(), Box<dyn std::error::Error>> {
    let key = format!("testing/{}", uuid::Uuid::new_v4());
    let test_data = vec![42u8; 100];

    // Put object
    client
        .put_object(&key, bytes::Bytes::from(test_data.clone()), 1)
        .await?;

    // Get object
    let read_data = client.get_object(&key).await?;
    if read_data != test_data {
        return Err("Read data does not match written data".into());
    }

    // Delete object
    client.delete_object(&key).await?;
    Ok(())
}

async fn run_app(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Commands::Format {
            name,
            block_size,
            capacity,
            mem_cache_size,
            disk_cache_size,
            disk_cache_paths,
            s3_endpoint,
            s3_access_key,
            s3_secret_key,
            s3_bucket,
            force,
            quick,
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
            let redis_url = &cli.garnet_url;

            // Check if active clients are connected to the filesystem
            if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                    let raw_clients: std::collections::HashMap<String, String> = con
                        .hgetall("squeezefs:active_clients")
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
                }
            }

            // Check if squeezefs volume is already formatted on the database
            if !force {
                if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                    if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                        let exists_format: bool =
                            con.exists("squeezefs:format").await.unwrap_or(false);
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

            // 4. S3 parameters consistency
            let has_s3_endpoint = s3_endpoint.is_some()
                || std::env::var("RUSTFS_ENDPOINT").is_ok()
                || std::env::var("AWS_ENDPOINT_URL").is_ok();

            let has_other_s3_params = s3_access_key.is_some()
                || s3_secret_key.is_some()
                || s3_bucket.is_some()
                || std::env::var("RUSTFS_ACCESS_KEY").is_ok()
                || std::env::var("AWS_ACCESS_KEY_ID").is_ok()
                || std::env::var("RUSTFS_SECRET_KEY").is_ok()
                || std::env::var("AWS_SECRET_ACCESS_KEY").is_ok()
                || std::env::var("RUSTFS_BUCKET").is_ok()
                || std::env::var("AWS_BUCKET").is_ok();

            if has_other_s3_params && !has_s3_endpoint {
                return Err("S3 bucket or credential parameters were provided, but no S3 endpoint was specified. \
                             If you want to use S3 storage, you must provide --s3-endpoint or set the RUSTFS_ENDPOINT environment variable. \
                             If you intended to use the in-memory mock store, please omit all S3 parameters.".into());
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
            let parsed_capacity = parse_human_readable_size(&capacity)?;

            squeezefs::cache::parse_duration(&upload_delay)?;

            if !has_s3_endpoint {
                println!(
                    "{} {}",
                    "WARNING:".yellow().bold(),
                    "No S3 endpoint provided. SqueezeFS will fall back to an IN-MEMORY mock store. ALL DATA WILL BE LOST when the mount daemon terminates."
                        .yellow()
                );
            }

            squeezefs::fuse_client::format_volume_ext(
                redis_url,
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
                s3_endpoint.as_deref(),
                s3_access_key.as_deref(),
                s3_secret_key.as_deref(),
                s3_bucket.as_deref(),
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
            let status = squeezefs::fuse_client::get_volume_status(redis_url).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Commands::Status => {
            let redis_url = &cli.garnet_url;
            let status = squeezefs::fuse_client::get_volume_status(redis_url).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Commands::Mount {
            mountpoint,
            mem_cache_size,
            disk_cache_size,
            disk_cache_paths,
            local_ips,
            s3_endpoint,
            s3_access_key,
            s3_secret_key,
            s3_bucket,
            daemon: _,
            uid,
            gid,
            p2p_addr,
            no_writeback,
            max_background_uploads,
            allow_other,
            check_storage,
            options,
            read_cache_size,
            write_cache_size,
            read_mem_cache_size,
            write_mem_cache_size,
            dismount_wait,
            upload_delay,
            fuse_io_uring_sqpoll_idle_ms,
            fuse_io_uring_sqpoll_cpu,
        } => {
            let writeback = !no_writeback;
            let redis_url = &cli.garnet_url;

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
                    con.hgetall("squeezefs:format").await.unwrap_or_default()
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
                        .arg("squeezefs:format")
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
                } else {
                    dirs
                }
            } else if let Some(paths_str) = format_fields.get("disk_cache_paths") {
                if paths_str.is_empty() {
                    vec![get_default_staging_dir()]
                } else {
                    paths_str.split(',').map(PathBuf::from).collect()
                }
            } else {
                vec![get_default_staging_dir()]
            };

            let mut active_staging_dirs = Vec::new();
            if let Ok(mut con) = dlm.meta_client().get_connection().await {
                let status_map: std::collections::HashMap<String, String> = con
                    .hgetall("squeezefs:diskcache:status")
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

            log::info!("Resolving backend object store configuration...");
            if s3_endpoint.is_some()
                || s3_access_key.is_some()
                || s3_secret_key.is_some()
                || s3_bucket.is_some()
            {
                return Err("Backend overrides on mount are not allowed. Please configure backends using 'squeezefs backend'.".into());
            }

            log::info!("Resolving backend object store configuration...");
            // Load S3 settings: Env variables > Garnet stored settings
            let final_s3_endpoint = std::env::var("RUSTFS_ENDPOINT")
                .ok()
                .or_else(|| std::env::var("AWS_ENDPOINT_URL").ok())
                .or_else(|| {
                    format_fields
                        .get("s3_endpoint")
                        .filter(|s| !s.is_empty())
                        .cloned()
                });
            let final_s3_access_key = std::env::var("RUSTFS_ACCESS_KEY")
                .ok()
                .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok())
                .or_else(|| {
                    format_fields
                        .get("s3_access_key")
                        .filter(|s| !s.is_empty())
                        .cloned()
                });
            let final_s3_secret_key = std::env::var("RUSTFS_SECRET_KEY")
                .ok()
                .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok())
                .or_else(|| {
                    format_fields
                        .get("s3_secret_key")
                        .filter(|s| !s.is_empty())
                        .cloned()
                });
            let final_s3_bucket = std::env::var("RUSTFS_BUCKET").ok().or_else(|| {
                format_fields
                    .get("s3_bucket")
                    .filter(|s| !s.is_empty())
                    .cloned()
            });

            // Retrieve registered backends or initialize the default one
            let multi_backend = MultiBackendClient::new();

            // Read all registered backends and their statuses from Garnet
            if let Ok(mut con) = dlm.meta_client().get_connection().await {
                let backends_map: std::collections::HashMap<String, String> =
                    con.hgetall("squeezefs:backends").await.unwrap_or_default();
                let statuses_map: std::collections::HashMap<String, String> = con
                    .hgetall("squeezefs:backend:status")
                    .await
                    .unwrap_or_default();

                for (be_id, be_json) in backends_map {
                    if let Ok(config) = serde_json::from_str::<serde_json::Value>(&be_json) {
                        let ep = config["endpoint"].as_str().map(|s| s.to_string());
                        let ak = config["access_key"].as_str().map(|s| s.to_string());
                        let sk = config["secret_key"].as_str().map(|s| s.to_string());
                        let bu = config["bucket"].as_str().map(|s| s.to_string());

                        log::info!(
                            "Registering storage backend: {} (S3 endpoint: {:?}, bucket: {:?})",
                            be_id,
                            ep.as_deref().unwrap_or("default"),
                            bu.as_deref().unwrap_or("squeezefs-data")
                        );

                        let client = RustFsClient::new_with_local_ips(
                            local_ips.clone().unwrap_or_default(),
                            ep,
                            ak,
                            sk,
                            bu,
                        )
                        .await;
                        multi_backend.register_backend(&be_id, client);

                        if let Some(status) = statuses_map.get(&be_id) {
                            multi_backend.set_backend_status(&be_id, status);
                            log::info!("Set storage backend status: {} -> {}", be_id, status);
                        }
                    }
                }

                for (be_id, status) in statuses_map {
                    multi_backend.set_backend_status(&be_id, &status);
                }
            }

            // Determine active write backend
            let active_be_id = format_fields
                .get("active_write_backend")
                .cloned()
                .unwrap_or_else(|| "backend_0".to_string());

            // Ensure we have at least backend_0 registered if no backends were found
            if !multi_backend.has_backend("backend_0") {
                log::info!("Registering default storage backend: backend_0");
                let default_client = RustFsClient::new_with_local_ips(
                    local_ips.clone().unwrap_or_default(),
                    final_s3_endpoint.clone(),
                    final_s3_access_key.clone(),
                    final_s3_secret_key.clone(),
                    final_s3_bucket.clone(),
                )
                .await;
                multi_backend.register_backend("backend_0", default_client);
            }

            multi_backend.set_active_backend_id(active_be_id.clone());
            log::info!("Active write storage backend set to: {}", active_be_id);

            let active_client = multi_backend
                .get_backend(&active_be_id)
                .or_else(|| multi_backend.get_backend("backend_0"))
                .ok_or("No storage backend client available")?;

            if check_storage {
                log::info!("Running storage connectivity check...");
                let start = std::time::Instant::now();
                if let Err(e) = test_storage(&active_client).await {
                    log::error!("Object storage check failed: {:?}", e);
                    return Err(format!("Object storage check failed: {:?}", e).into());
                } else {
                    log::info!("Object storage check passed in {:?}", start.elapsed());
                }
            }

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

            let masked_access_key = final_s3_access_key
                .as_ref()
                .map(|s| {
                    if s.is_empty() {
                        "none".to_string()
                    } else {
                        "******".to_string()
                    }
                })
                .unwrap_or_else(|| "none".to_string());
            let masked_secret_key = final_s3_secret_key
                .as_ref()
                .map(|s| {
                    if s.is_empty() {
                        "none".to_string()
                    } else {
                        "******".to_string()
                    }
                })
                .unwrap_or_else(|| "none".to_string());
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
                "endpoint": final_s3_endpoint.as_deref().unwrap_or(""),
                "access_key": masked_access_key,
                "secret_key": masked_secret_key,
                "bucket": final_s3_bucket.as_deref().unwrap_or("squeezefs-data"),
                "writeback": writeback,
                "allow_other": allow_other,
                "options": options,
                "compression": compression,
                "encrypt_algo": encrypt_algo,
                "encrypt_key": masked_encrypt_key,
            });
            let config_str = serde_json::to_string_pretty(&config_json).unwrap_or_default();
            log::info!("SqueezeFS version {}", env!("CARGO_PKG_VERSION"));
            log::info!(
                "Data use {:?}",
                final_s3_bucket.as_deref().unwrap_or("mock")
            );
            log::info!("SqueezeFS mount configuration:\n{}", config_str);

            // Run staging / active write recovery on mount startup
            for dir in &active_staging_dirs {
                log::info!("Running staging recovery on: {:?}", dir);
                let recovery_backend = multi_backend.get_backend("backend_0").unwrap();
                if let Err(e) =
                    squeezefs::recovery::recover_staging(dir, &recovery_backend, dlm.meta_client())
                        .await
                {
                    log::warn!("Staging recovery failed for {:?}: {:?}", dir, e);
                }
            }

            let cache = TieredCache::new(
                active_staging_dirs.clone(),
                Some(&resolved_read_mem_cache_size),
                Some(&resolved_write_mem_cache_size),
                Some(&resolved_read_cache_size),
                Some(&resolved_write_cache_size),
                multi_backend.clone().get_backend("backend_0").unwrap(), // Cache uses default backend for staging
                dlm.meta_client().clone(),
            )?;
            if let Some(ref addr) = p2p_addr {
                let _ = cache.nvme.p2p_addr.set(addr.clone());
            }
            let router = DataRouter::new(dlm.clone(), multi_backend, cache);
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
            fs_engine.max_background_uploads = max_background_uploads;

            apply_fuse_io_uring_sqpoll_env(
                resolved_fuse_io_uring_sqpoll_idle_ms,
                resolved_fuse_io_uring_sqpoll_cpu,
            );

            println!("Mounting Squeezefs at {:?}...", mountpoint);

            // Daemonization has already happened at the start of main() prior to Tokio runtime initialization.

            if let Some(ref addr) = p2p_addr {
                let server = squeezefs::p2p::P2pServer::new(
                    addr.clone(),
                    fs_engine.router.cache.nvme.clone(),
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
        Commands::Bench {
            path,
            threads,
            size,
            iterations,
        } => {
            let redis_url = &cli.garnet_url;
            for iter in 1..=iterations {
                if iterations > 1 {
                    println!("\n--- Benchmark Iteration {}/{} ---", iter, iterations);
                }
                run_benchmark(&path, threads, size, redis_url).await?;
            }
        }
        Commands::Clone { src, dest } => {
            let redis_url = &cli.garnet_url;
            let staging_dirs = vec![get_default_staging_dir()];

            let dlm = DlmClient::new(redis_url)?;
            let backend = RustFsClient::new().await;
            let multi_backend = MultiBackendClient::new();
            multi_backend.register_backend("backend_0", backend.clone());

            let cache = TieredCache::new(
                staging_dirs,
                None,
                None,
                None,
                None,
                backend,
                dlm.meta_client().clone(),
            )?;
            let router = DataRouter::new(dlm, multi_backend, cache);

            println!("Cloning file from {} to {}...", src, dest);
            router.clone_path(&src, &dest).await?;
            println!("File cloned successfully.");
        }
        Commands::Df { path } => {
            let redis_url = &cli.garnet_url;
            run_df_command(redis_url, path).await?;
        }
        Commands::Nvmeof { action } => match action {
            NvmeofActions::Share {
                backing_path,
                subnqn,
                port,
                ip,
            } => {
                let resolved_nqn = squeezefs::nvmeof::share_target(
                    &backing_path,
                    subnqn.as_deref(),
                    port,
                    &ip,
                )?;
                println!("Successfully shared '{}' as NVMe-oF target.", backing_path);
                println!("Subsystem NQN: {}", resolved_nqn);
                println!("Connection string for client nodes:");
                println!(
                    "  squeezefs nvmeof connect --ip <your-target-ip> --port {} --subnqn {}",
                    port, resolved_nqn
                );
            }
            NvmeofActions::Unshare { subnqn } => {
                squeezefs::nvmeof::unshare_target(&subnqn)?;
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
        },
        Commands::Tune => {
            tune_system()?;
        }
        Commands::Config {
            garnet_url,
            fs_name,
            action,
        } => match action {
            ConfigActions::Backend(action) => match action {
                BackendActions::Add {
                    name,
                    s3_endpoint,
                    s3_access_key,
                    s3_secret_key,
                    s3_bucket,
                } => {
                    squeezefs::config_ops::add_storage_backend(
                        &garnet_url,
                        &fs_name,
                        &name,
                        &s3_endpoint,
                        &s3_access_key,
                        &s3_secret_key,
                        &s3_bucket,
                    )
                    .await?;
                    println!("Storage backend '{}' added successfully.", name);
                }
                BackendActions::Remove { name, force } => {
                    squeezefs::config_ops::remove_storage_backend(
                        &garnet_url,
                        &fs_name,
                        &name,
                        force,
                    )
                    .await?;
                    println!("Storage backend '{}' removed successfully.", name);
                }
                BackendActions::Enable { name } => {
                    squeezefs::config_ops::enable_storage_backend(&garnet_url, &fs_name, &name)
                        .await?;
                    println!("Storage backend '{}' enabled successfully.", name);
                }
                BackendActions::Disable { name } => {
                    squeezefs::config_ops::disable_storage_backend(&garnet_url, &fs_name, &name)
                        .await?;
                    println!("Storage backend '{}' disabled successfully.", name);
                }
                BackendActions::List => {
                    let list = squeezefs::config_ops::list_config(&garnet_url, &fs_name).await?;
                    let mut output = serde_json::Map::new();
                    for (be_id, be_json_str) in &list.backends {
                        if let Ok(mut be_val) =
                            serde_json::from_str::<serde_json::Value>(be_json_str)
                        {
                            let status = list
                                .backend_statuses
                                .get(be_id)
                                .cloned()
                                .unwrap_or_else(|| "enabled".to_string());
                            if let Some(obj) = be_val.as_object_mut() {
                                obj.insert("status".to_string(), serde_json::Value::String(status));
                            }
                            output.insert(be_id.clone(), be_val);
                        }
                    }
                    if !list.backends.contains_key("backend_0") {
                        let status = list
                            .backend_statuses
                            .get("backend_0")
                            .cloned()
                            .unwrap_or_else(|| "enabled".to_string());
                        let be_val = serde_json::json!({
                            "endpoint": "",
                            "access_key": "admin",
                            "secret_key": "password",
                            "bucket": "squeezefs-data",
                            "status": status,
                        });
                        output.insert("backend_0".to_string(), be_val);
                    }
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::Value::Object(output))?
                    );
                }
            },
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
                    let list = squeezefs::config_ops::list_config(&garnet_url, &fs_name).await?;
                    println!("{}", serde_json::to_string_pretty(&list.diskcaches)?);
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
        },
        Commands::Umount { mountpoint, force } => {
            use std::io::IsTerminal;
            use std::io::Write;

            let redis_url = &cli.garnet_url;

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
                            .hget("squeezefs:format", "disk_cache_paths")
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

            // 4. Resolve dismount_wait limit (default: 10)
            let mut dismount_wait = 10;
            if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                    let wait_str: Option<String> = con
                        .hget("squeezefs:format", "dismount_wait")
                        .await
                        .unwrap_or(None);
                    if let Some(s) = wait_str {
                        if let Ok(w) = s.parse::<u64>() {
                            dismount_wait = w;
                        }
                    }
                }
            }

            // 5. Count staged files and active writes from cache segments
            let mut max_write_bytes = 100 * 1024 * 1024;
            if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                    let size_str: Option<String> = con
                        .hget("squeezefs:format", "write_disk_limit")
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
                println!("  [w] Wait for staged files to drain/flush to S3 (recommended)");
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

            match status {
                Ok(s) if s.success() => {
                    println!("Successfully unmounted mountpoint {:?}", mountpoint);
                }
                _ => {
                    let umount_status = std::process::Command::new("umount")
                        .arg(&mountpoint)
                        .status();
                    match umount_status {
                        Ok(s) if s.success() => {
                            println!("Successfully unmounted mountpoint {:?}", mountpoint);
                        }
                        _ => {
                            eprintln!(
                                "Error: Failed to unmount mountpoint {:?}. Try running with sudo.",
                                mountpoint
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    }

    Ok(())
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
                    if path.join(".stats").exists() && path.join(".config").exists() {
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
        let dir_key = format!("squeezefs:dir:{}", current_ino);
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
    let dlm = DlmClient::new(redis_url)?;
    let mut con = dlm.get_connection().await?;

    let mounts = find_squeezefs_mounts();

    match path_opt {
        None => {
            let format_fields: HashMap<String, String> =
                con.hgetall("squeezefs:format").await.unwrap_or_default();
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
                .hgetall("squeezefs:block_sizes")
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
            println!("Physical S3 Size:    {}", format_size(total_physical_size));
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
                    if let Ok(stats_str) = std::fs::read_to_string(&stats_path) {
                        if let Ok(stats_json) =
                            serde_json::from_str::<serde_json::Value>(&stats_str)
                        {
                            let cap = &stats_json["cache_capacities"];
                            let ram_read_curr = cap["read_lru_current_bytes"].as_u64().unwrap_or(0);
                            let ram_read_max = cap["read_lru_max_bytes"].as_u64().unwrap_or(0);
                            let ram_write_curr =
                                cap["write_lru_current_bytes"].as_u64().unwrap_or(0);
                            let ram_write_max = cap["write_lru_max_bytes"].as_u64().unwrap_or(0);
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
                }
            }
        }
        Some(path_str) => {
            let abs_path = std::path::Path::new(&path_str)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(&path_str));

            let mut matching_mount = None;
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
                            locations.push("S3");
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
                    let block_map_key = format!("squeezefs:block_map:{}", map_id);
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
                                con.hget("squeezefs:block_sizes", bk).await?;
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
                                locations.push("S3");
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
}

async fn get_daemon_metrics(redis_url: &str) -> Option<HashMap<String, u64>> {
    let client = squeezefs::dlm::MetaClient::new(redis_url).ok()?;
    let mut con = client.get_connection().await.ok()?;
    let metrics: HashMap<String, String> = con.hgetall("metrics:daemon").await.ok()?;

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
    size_mb: usize,
    redis_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("Benchmark path {:?} does not exist", path).into());
    }

    println!(
        "{}",
        "==================================================".bold()
    );
    println!(
        "  Running Squeezefs Benchmark (T={}, Size={}MB)",
        threads, size_mb
    );
    println!(
        "{}",
        "==================================================".bold()
    );

    // 1. Fetch baseline metrics
    let baseline_metrics = get_daemon_metrics(redis_url).await;

    let mp = MultiProgress::new();
    let pb_style = ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}")?
        .progress_chars("#>-");

    // Pre-calculate byte size
    let big_file_bytes = size_mb * 1024 * 1024;
    let chunk_size = 1024 * 1024; // 1MB block size
    let num_chunks = big_file_bytes / chunk_size;

    // --- WRITE BIG FILE ---
    println!("Writing big file ({} MB/thread)...", size_mb);
    let pb_write_big = mp.add(ProgressBar::new((threads * num_chunks) as u64));
    pb_write_big.set_style(pb_style.clone());
    pb_write_big.set_message("Write Big File");

    let t_start = Instant::now();
    let mut write_tasks = Vec::new();
    for t_id in 0..threads {
        let path_clone = path.to_path_buf();
        let pb = pb_write_big.clone();
        write_tasks.push(tokio::spawn(async move {
            let file_path = path_clone.join(format!("bench_big_{}.bin", t_id));
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&file_path)
                .await?;
            let buf = vec![0u8; chunk_size];
            for _ in 0..num_chunks {
                file.write_all(&buf).await?;
                pb.inc(1);
            }
            file.sync_all().await?;
            Ok::<_, std::io::Error>(())
        }));
    }

    for task in write_tasks {
        task.await??;
    }
    pb_write_big.finish_with_message("Done");
    let d_write_big = t_start.elapsed();

    // --- READ BIG FILE ---
    println!("Reading big file...");
    let pb_read_big = mp.add(ProgressBar::new((threads * num_chunks) as u64));
    pb_read_big.set_style(pb_style.clone());
    pb_read_big.set_message("Read Big File");

    let t_start = Instant::now();
    let mut read_tasks = Vec::new();
    for t_id in 0..threads {
        let path_clone = path.to_path_buf();
        let pb = pb_read_big.clone();
        read_tasks.push(tokio::spawn(async move {
            let file_path = path_clone.join(format!("bench_big_{}.bin", t_id));
            let mut file = fs::File::open(&file_path).await?;
            let mut buf = vec![0u8; chunk_size];
            for _ in 0..num_chunks {
                file.read_exact(&mut buf).await?;
                pb.inc(1);
            }
            Ok::<_, std::io::Error>(())
        }));
    }

    for task in read_tasks {
        task.await??;
    }
    pb_read_big.finish_with_message("Done");
    let d_read_big = t_start.elapsed();

    // --- WRITE SMALL FILES ---
    let small_files_count = 100;
    let small_file_bytes = 128 * 1024; // 128 KB
    println!(
        "Writing small files ({} files of 128 KB per thread)...",
        small_files_count
    );
    let pb_write_small = mp.add(ProgressBar::new((threads * small_files_count) as u64));
    pb_write_small.set_style(pb_style.clone());
    pb_write_small.set_message("Write Small Files");

    let t_start = Instant::now();
    let mut write_small_tasks = Vec::new();
    for t_id in 0..threads {
        let path_clone = path.to_path_buf();
        let pb = pb_write_small.clone();
        write_small_tasks.push(tokio::spawn(async move {
            let buf = vec![1u8; small_file_bytes];
            for f_id in 0..small_files_count {
                let file_path = path_clone.join(format!("bench_small_{}_{}.bin", t_id, f_id));
                let mut file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&file_path)
                    .await?;
                file.write_all(&buf).await?;
                file.sync_all().await?;
                pb.inc(1);
            }
            Ok::<_, std::io::Error>(())
        }));
    }

    for task in write_small_tasks {
        task.await??;
    }
    pb_write_small.finish_with_message("Done");
    let d_write_small = t_start.elapsed();

    // --- READ SMALL FILES ---
    println!("Reading small files...");
    let pb_read_small = mp.add(ProgressBar::new((threads * small_files_count) as u64));
    pb_read_small.set_style(pb_style.clone());
    pb_read_small.set_message("Read Small Files");

    let t_start = Instant::now();
    let mut read_small_tasks = Vec::new();
    for t_id in 0..threads {
        let path_clone = path.to_path_buf();
        let pb = pb_read_small.clone();
        read_small_tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; small_file_bytes];
            for f_id in 0..small_files_count {
                let file_path = path_clone.join(format!("bench_small_{}_{}.bin", t_id, f_id));
                let mut file = fs::File::open(&file_path).await?;
                file.read_exact(&mut buf).await?;
                pb.inc(1);
            }
            Ok::<_, std::io::Error>(())
        }));
    }

    for task in read_small_tasks {
        task.await??;
    }
    pb_read_small.finish_with_message("Done");
    let d_read_small = t_start.elapsed();

    // --- STAT SMALL FILES ---
    println!("Stat small files...");
    let pb_stat_small = mp.add(ProgressBar::new((threads * small_files_count) as u64));
    pb_stat_small.set_style(pb_style.clone());
    pb_stat_small.set_message("Stat Files");

    let t_start = Instant::now();
    let mut stat_tasks = Vec::new();
    for t_id in 0..threads {
        let path_clone = path.to_path_buf();
        let pb = pb_stat_small.clone();
        stat_tasks.push(tokio::spawn(async move {
            for f_id in 0..small_files_count {
                let file_path = path_clone.join(format!("bench_small_{}_{}.bin", t_id, f_id));
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
    let d_stat = t_start.elapsed();

    // --- DELETE FILES ---
    println!("Deleting small files...");
    let pb_delete_small = mp.add(ProgressBar::new((threads * small_files_count) as u64));
    pb_delete_small.set_style(pb_style.clone());
    pb_delete_small.set_message("Delete Files");

    let t_start = Instant::now();
    let mut delete_tasks = Vec::new();
    for t_id in 0..threads {
        let path_clone = path.to_path_buf();
        let pb = pb_delete_small.clone();
        delete_tasks.push(tokio::spawn(async move {
            for f_id in 0..small_files_count {
                let file_path = path_clone.join(format!("bench_small_{}_{}.bin", t_id, f_id));
                let _ = fs::remove_file(&file_path).await;
                pb.inc(1);
            }
            Ok::<_, std::io::Error>(())
        }));
    }

    for task in delete_tasks {
        task.await??;
    }
    pb_delete_small.finish_with_message("Done");
    let d_delete = t_start.elapsed();

    // Clean up big files
    for t_id in 0..threads {
        let file_path = path.join(format!("bench_big_{}.bin", t_id));
        let _ = fs::remove_file(file_path).await;
    }

    // 2. Fetch post-benchmark metrics
    let post_metrics = get_daemon_metrics(redis_url).await;

    // --- CALCULATE PERFORMANCE VALUES ---
    let total_big_bytes = (threads * big_file_bytes) as f64;
    let write_big_tput = (total_big_bytes / (1024.0 * 1024.0)) / d_write_big.as_secs_f64();
    let write_big_cost = (d_write_big.as_secs_f64() * 1000.0) / threads as f64;

    let read_big_tput = (total_big_bytes / (1024.0 * 1024.0)) / d_read_big.as_secs_f64();
    let read_big_cost = (d_read_big.as_secs_f64() * 1000.0) / threads as f64;

    let total_small_files = (threads * small_files_count) as f64;
    let write_small_rate = total_small_files / d_write_small.as_secs_f64();
    let write_small_cost = (d_write_small.as_secs_f64() * 1000.0) / total_small_files;

    let read_small_rate = total_small_files / d_read_small.as_secs_f64();
    let read_small_cost = (d_read_small.as_secs_f64() * 1000.0) / total_small_files;

    let stat_rate = total_small_files / d_stat.as_secs_f64();
    let stat_cost = (d_stat.as_secs_f64() * 1000.0) / total_small_files;

    let delete_rate = total_small_files / d_delete.as_secs_f64();
    let delete_cost = (d_delete.as_secs_f64() * 1000.0) / total_small_files;

    // --- COLOR THRESHOLDS ---
    let format_big_val = |val: f64| {
        let s = format!("{:>12.2} MiB/s", val);
        if val > 100.0 {
            s.green()
        } else if val > 50.0 {
            s.yellow()
        } else {
            s.red()
        }
    };
    let format_small_val = |val: f64| {
        let s = format!("{:>12.2} ops/s", val);
        if val > 200.0 {
            s.green()
        } else if val > 100.0 {
            s.yellow()
        } else {
            s.red()
        }
    };
    let format_stat_val = |val: f64| {
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
        "+--------------------+--------------------+--------------------+".bold()
    );
    println!("| {:<18} | {:<18} | {:<18} |", "ITEM", "VALUE", "COST");
    println!(
        "{}",
        "+--------------------+--------------------+--------------------+".bold()
    );
    println!(
        "| {:<18} | {} | {:>14.2} ms/op |",
        "Write big file",
        format_big_val(write_big_tput),
        write_big_cost
    );
    println!(
        "| {:<18} | {} | {:>14.2} ms/op |",
        "Read big file",
        format_big_val(read_big_tput),
        read_big_cost
    );
    println!(
        "| {:<18} | {} | {:>14.2} ms/op |",
        "Write small files",
        format_small_val(write_small_rate),
        write_small_cost
    );
    println!(
        "| {:<18} | {} | {:>14.2} ms/op |",
        "Read small files",
        format_small_val(read_small_rate),
        read_small_cost
    );
    println!(
        "| {:<18} | {} | {:>14.2} ms/op |",
        "Stat files",
        format_stat_val(stat_rate),
        stat_cost
    );
    println!(
        "| {:<18} | {} | {:>14.2} ms/op |",
        "Delete files",
        format_stat_val(delete_rate),
        delete_cost
    );
    println!(
        "{}",
        "+--------------------+--------------------+--------------------+".bold()
    );

    // --- RENDER DAEMON METRICS ---
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
        show_metric("S3 Put Object", "put_obj");
        show_metric("S3 Get Object", "get_obj");
        show_metric("S3 Delete Object", "del_obj");
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
                                "Applying optimized read_ahead_kb = 16384 (16MB) for connection {}...",
                                conn_id_str
                            );
                            let _ = std::fs::write(bdi_path, "16384\n");
                        }
                    }
                }
            }
        }
    }

    println!("=== Auto-tuning completed ===\n");
    Ok(())
}
