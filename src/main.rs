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
        /// Block size in bytes (default: 4MB)
        #[arg(long, default_value_t = 4194304)]
        block_size: u64,
        /// Maximum capacity of the volume in bytes (default: 1PB)
        #[arg(long, default_value_t = 1024 * 1024 * 1024 * 1024 * 1024)]
        capacity: u64,
        /// Memory cache limit (default: "1GB")
        #[arg(long)]
        mem_cache_size: Option<String>,
        /// Disk cache limit (default: "10GB")
        #[arg(long)]
        disk_cache_size: Option<String>,
        /// Comma-separated paths to local staging/cache directories
        #[arg(long, value_delimiter = ',')]
        disk_cache_paths: Option<Vec<PathBuf>>,
        /// S3 compatible object store endpoint url
        #[arg(long)]
        s3_endpoint: Option<String>,
        /// S3 compatible object store access key
        #[arg(long)]
        s3_access_key: Option<String>,
        /// S3 compatible object store secret key
        #[arg(long)]
        s3_secret_key: Option<String>,
        /// S3 compatible object store bucket name
        #[arg(long)]
        s3_bucket: Option<String>,
        /// Force formatting even if a squeezefs volume is already detected
        #[arg(long, short = 'f')]
        force: bool,
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
        #[arg(long)]
        disk_cache_size: Option<String>,

        /// Comma-separated paths to local staging/cache directories
        #[arg(long, value_delimiter = ',')]
        disk_cache_paths: Option<Vec<PathBuf>>,

        /// Comma-separated list of local source IP interfaces for multi-rail connection bonding
        #[arg(long, value_delimiter = ',')]
        local_ips: Option<Vec<std::net::IpAddr>>,

        /// S3 compatible object store endpoint url (overrides stored configuration)
        #[arg(long)]
        s3_endpoint: Option<String>,
        /// S3 compatible object store access key (overrides stored configuration)
        #[arg(long)]
        s3_access_key: Option<String>,
        /// S3 compatible object store secret key (overrides stored configuration)
        #[arg(long)]
        s3_secret_key: Option<String>,
        /// S3 compatible object store bucket name (overrides stored configuration)
        #[arg(long)]
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
}

#[derive(Subcommand, Debug, Clone)]
enum ConfigActions {
    /// Add a staging disk cache or storage backend
    Add {
        /// Category: must be "diskcache" or "backend"
        category: String,
        /// Value: path for diskcache, or backend_id for backend
        value: String,

        /// S3 compatible object store endpoint url (only for backend category)
        #[arg(long)]
        s3_endpoint: Option<String>,
        /// S3 compatible object store access key (only for backend category)
        #[arg(long)]
        s3_access_key: Option<String>,
        /// S3 compatible object store secret key (only for backend category)
        #[arg(long)]
        s3_secret_key: Option<String>,
        /// S3 compatible object store bucket name (only for backend category)
        #[arg(long)]
        s3_bucket: Option<String>,
    },
    /// Remove a staging disk cache or storage backend
    Remove {
        /// Category: must be "diskcache" or "backend"
        category: String,
        /// Value: path for diskcache, or backend_id for backend
        value: String,
        /// Force removal ignoring safety checks
        #[arg(long)]
        force: bool,
    },
    /// Enable a staging disk cache
    Enable {
        /// Category: must be "diskcache"
        category: String,
        /// Path to enable
        value: String,
    },
    /// Disable a staging disk cache
    Disable {
        /// Category: must be "diskcache"
        category: String,
        /// Path to disable
        value: String,
    },
    /// Flush a disabled staging disk cache
    Flush {
        /// Category: must be "diskcache"
        category: String,
        /// Path to flush
        value: String,
    },
    /// Set the active storage backend for writes
    #[command(name = "set-active-backend")]
    SetActiveBackend {
        /// Backend ID to set active
        backend_id: String,
    },
    /// List current configuration (diskcaches, backends, active backend)
    List,
    /// Consistency check on metadata and block references
    Fsck,
}

#[cfg(feature = "dhat-on")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(all(target_os = "linux", not(feature = "dhat-on")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "dhat-on")]
    let _profiler = dhat::Profiler::new_heap();

    let cli = Cli::parse();

    #[cfg(unix)]
    if let Commands::Mount { daemon: true, .. } = &cli.command {
        unsafe {
            let pid = libc::fork();
            if pid < 0 {
                eprintln!("Failed to fork daemon process");
                std::process::exit(1);
            } else if pid > 0 {
                println!("Squeezefs daemon started (PID: {})", pid);
                std::process::exit(0);
            }
            // Child process detaches
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
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
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

    rt.block_on(async {
        run_app(cli).await
    })
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
        } => {
            let redis_url = &cli.garnet_url;

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

            squeezefs::fuse_client::format_volume(
                redis_url,
                &name,
                block_size,
                capacity,
                mem_cache_size.as_deref(),
                disk_cache_size.as_deref(),
                disk_cache_paths.as_deref(),
                s3_endpoint.as_deref(),
                s3_access_key.as_deref(),
                s3_secret_key.as_deref(),
                s3_bucket.as_deref(),
            )
            .await?;
            println!("Volume '{}' formatted successfully.", name);
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
        } => {
            let redis_url = &cli.garnet_url;

            println!("Initializing metadata client...");
            let dlm =
                DlmClient::new_with_local_ips(redis_url, local_ips.clone().unwrap_or_default())
                    .await?;

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
                .or_else(|| format_fields.get("mem_cache_size").cloned())
                .unwrap_or_else(|| "1GB".to_string());

            // Resolve disk cache size: CLI override > Garnet setting > default "10GB"
            let resolved_disk_cache_size = disk_cache_size
                .or_else(|| format_fields.get("disk_cache_size").cloned())
                .unwrap_or_else(|| "10GB".to_string());

            // Resolve staging directories: CLI override > Garnet setting > default "/tmp/squeezefs_staging"
            let staging_dirs = if let Some(dirs) = disk_cache_paths {
                if dirs.is_empty() {
                    vec![PathBuf::from("/tmp/squeezefs_staging")]
                } else {
                    dirs
                }
            } else if let Some(paths_str) = format_fields.get("disk_cache_paths") {
                if paths_str.is_empty() {
                    vec![PathBuf::from("/tmp/squeezefs_staging")]
                } else {
                    paths_str.split(',').map(PathBuf::from).collect()
                }
            } else {
                vec![PathBuf::from("/tmp/squeezefs_staging")]
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

            for dir in &active_staging_dirs {
                fs::create_dir_all(dir).await?;
            }

            // Tune host parameters automatically
            let _ = tune_system();

            println!("Resolving backend object store configuration...");
            // Load S3 settings: CLI override > Env variables > Garnet stored settings
            let final_s3_endpoint = s3_endpoint
                .or_else(|| std::env::var("RUSTFS_ENDPOINT").ok())
                .or_else(|| format_fields.get("s3_endpoint").cloned());
            let final_s3_access_key = s3_access_key
                .or_else(|| std::env::var("RUSTFS_ACCESS_KEY").ok())
                .or_else(|| format_fields.get("s3_access_key").cloned());
            let final_s3_secret_key = s3_secret_key
                .or_else(|| std::env::var("RUSTFS_SECRET_KEY").ok())
                .or_else(|| format_fields.get("s3_secret_key").cloned());
            let final_s3_bucket = s3_bucket
                .or_else(|| std::env::var("RUSTFS_BUCKET").ok())
                .or_else(|| format_fields.get("s3_bucket").cloned());

            // Retrieve registered backends or initialize the default one
            let multi_backend = MultiBackendClient::new();

            // Read all registered backends from Garnet
            if let Ok(mut con) = dlm.meta_client().get_connection().await {
                let backends_map: std::collections::HashMap<String, String> =
                    con.hgetall("squeezefs:backends").await.unwrap_or_default();

                for (be_id, be_json) in backends_map {
                    if let Ok(config) = serde_json::from_str::<serde_json::Value>(&be_json) {
                        let ep = config["endpoint"].as_str().map(|s| s.to_string());
                        let ak = config["access_key"].as_str().map(|s| s.to_string());
                        let sk = config["secret_key"].as_str().map(|s| s.to_string());
                        let bu = config["bucket"].as_str().map(|s| s.to_string());

                        let client = RustFsClient::new_with_local_ips(
                            local_ips.clone().unwrap_or_default(),
                            ep,
                            ak,
                            sk,
                            bu,
                        )
                        .await;
                        multi_backend.register_backend(&be_id, client);
                    }
                }
            }

            // Determine active write backend
            let mut active_be_id = format_fields
                .get("active_write_backend")
                .cloned()
                .unwrap_or_else(|| "backend_0".to_string());

            // If we have CLI or env overrides for the backend, register it dynamically
            if final_s3_endpoint.is_some()
                || final_s3_access_key.is_some()
                || final_s3_secret_key.is_some()
                || final_s3_bucket.is_some()
            {
                // Generate a custom ID for this runtime backend, e.g. "backend_override"
                active_be_id = "backend_override".to_string();
                let override_client = RustFsClient::new_with_local_ips(
                    local_ips.clone().unwrap_or_default(),
                    final_s3_endpoint.clone(),
                    final_s3_access_key.clone(),
                    final_s3_secret_key.clone(),
                    final_s3_bucket.clone(),
                )
                .await;

                // Initialize bucket
                if final_s3_endpoint.is_some() {
                    let _ = override_client.init_bucket().await;
                }

                multi_backend.register_backend(&active_be_id, override_client);

                // Save to Garnet registry so other mounting nodes can read it
                if let Ok(mut con) = dlm.meta_client().get_connection().await {
                    let backend_json = serde_json::json!({
                        "endpoint": final_s3_endpoint.clone().unwrap_or_default(),
                        "access_key": final_s3_access_key.clone().unwrap_or_else(|| "admin".to_string()),
                        "secret_key": final_s3_secret_key.clone().unwrap_or_else(|| "password".to_string()),
                        "bucket": final_s3_bucket.clone().unwrap_or_else(|| "squeezefs-data".to_string()),
                    }).to_string();
                    let _: () = redis::pipe()
                        .hset("squeezefs:backends", &active_be_id, backend_json)
                        .hset("squeezefs:format", "active_write_backend", &active_be_id)
                        .query_async(&mut con)
                        .await
                        .unwrap_or(());
                }
            }

            // Ensure we have at least backend_0 registered if no backends were found
            if !multi_backend.has_backend("backend_0") {
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

            multi_backend.set_active_backend_id(active_be_id);

            let cache = TieredCache::new(
                active_staging_dirs,
                Some(&resolved_mem_cache_size),
                Some(&resolved_disk_cache_size),
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

            let fs_engine = SqueezefsFilesystem::new(router, dlm, resolved_uid, resolved_gid);

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

            start_mount(mountpoint, fs_engine, resolved_uid, resolved_gid).await?;
        }
        Commands::Bench {
            path,
            threads,
            size,
        } => {
            let redis_url = &cli.garnet_url;
            run_benchmark(&path, threads, size, redis_url).await?;
        }
        Commands::Clone { src, dest } => {
            let redis_url = &cli.garnet_url;
            let staging_dirs = vec![PathBuf::from("/tmp/squeezefs_staging")];

            let dlm = DlmClient::new(redis_url)?;
            let backend = RustFsClient::new().await;
            let multi_backend = MultiBackendClient::new();
            multi_backend.register_backend("backend_0", backend.clone());

            let cache =
                TieredCache::new(staging_dirs, None, None, backend, dlm.meta_client().clone())?;
            let router = DataRouter::new(dlm, multi_backend, cache);

            println!("Cloning file from {} to {}...", src, dest);
            router.clone_path(&src, &dest).await?;
            println!("File cloned successfully.");
        }
        Commands::Tune => {
            tune_system()?;
        }
        Commands::Config {
            garnet_url,
            fs_name,
            action,
        } => match action {
            ConfigActions::Add {
                category,
                value,
                s3_endpoint,
                s3_access_key,
                s3_secret_key,
                s3_bucket,
            } => {
                if category == "diskcache" {
                    squeezefs::config_ops::add_disk_cache_path(
                        &garnet_url,
                        &fs_name,
                        Path::new(&value),
                    )
                    .await?;
                    println!("Disk cache path '{}' added successfully.", value);
                } else if category == "backend" {
                    let ep = s3_endpoint.unwrap_or_default();
                    let ak = s3_access_key.unwrap_or_else(|| "admin".to_string());
                    let sk = s3_secret_key.unwrap_or_else(|| "password".to_string());
                    let bu = s3_bucket.unwrap_or_else(|| "squeezefs-data".to_string());
                    squeezefs::config_ops::add_storage_backend(
                        &garnet_url,
                        &fs_name,
                        &value,
                        &ep,
                        &ak,
                        &sk,
                        &bu,
                    )
                    .await?;
                    println!("Storage backend '{}' added successfully.", value);
                } else {
                    eprintln!(
                        "Invalid category '{}'. Must be 'diskcache' or 'backend'.",
                        category
                    );
                    std::process::exit(1);
                }
            }
            ConfigActions::Remove {
                category,
                value,
                force,
            } => {
                if category == "diskcache" {
                    squeezefs::config_ops::remove_disk_cache_path(
                        &garnet_url,
                        &fs_name,
                        Path::new(&value),
                        force,
                    )
                    .await?;
                    println!("Disk cache path '{}' removed successfully.", value);
                } else if category == "backend" {
                    squeezefs::config_ops::remove_storage_backend(
                        &garnet_url,
                        &fs_name,
                        &value,
                        force,
                    )
                    .await?;
                    println!("Storage backend '{}' removed successfully.", value);
                } else {
                    eprintln!(
                        "Invalid category '{}'. Must be 'diskcache' or 'backend'.",
                        category
                    );
                    std::process::exit(1);
                }
            }
            ConfigActions::Enable { category, value } => {
                if category == "diskcache" {
                    squeezefs::config_ops::enable_disk_cache_path(
                        &garnet_url,
                        &fs_name,
                        Path::new(&value),
                    )
                    .await?;
                    println!("Disk cache path '{}' enabled successfully.", value);
                } else {
                    eprintln!("Invalid category '{}'. Must be 'diskcache'.", category);
                    std::process::exit(1);
                }
            }
            ConfigActions::Disable { category, value } => {
                if category == "diskcache" {
                    squeezefs::config_ops::disable_disk_cache_path(
                        &garnet_url,
                        &fs_name,
                        Path::new(&value),
                    )
                    .await?;
                    println!("Disk cache path '{}' disabled successfully.", value);
                } else {
                    eprintln!("Invalid category '{}'. Must be 'diskcache'.", category);
                    std::process::exit(1);
                }
            }
            ConfigActions::Flush { category, value } => {
                if category == "diskcache" {
                    squeezefs::config_ops::flush_disk_cache_path(
                        &garnet_url,
                        &fs_name,
                        Path::new(&value),
                    )
                    .await?;
                    println!("Disk cache path '{}' flushed successfully.", value);
                } else {
                    eprintln!("Invalid category '{}'. Must be 'diskcache'.", category);
                    std::process::exit(1);
                }
            }
            ConfigActions::SetActiveBackend { backend_id } => {
                squeezefs::config_ops::set_active_backend(&garnet_url, &fs_name, &backend_id)
                    .await?;
                println!("Active write backend set to '{}'.", backend_id);
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
        },
    }

    Ok(())
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
            }
        }
    }

    println!("=== Auto-tuning completed ===\n");
    Ok(())
}
