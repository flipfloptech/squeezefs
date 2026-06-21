use clap::{Parser, Subcommand};
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
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
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Mount {
            mountpoint,
            mem_cache_size,
            disk_cache_size,
            disk_cache_paths,
        } => {
            let redis_url = std::env::var("GARNET_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
            let staging_dirs = if let Some(dirs) = disk_cache_paths {
                if dirs.is_empty() {
                    vec![PathBuf::from("/tmp/squeezefs_staging")]
                } else {
                    dirs
                }
            } else {
                vec![PathBuf::from("/tmp/squeezefs_staging")]
            };

            for dir in &staging_dirs {
                fs::create_dir_all(dir).await?;
            }

            println!("Initializing distributed clients...");
            let dlm = DlmClient::new(&redis_url)?;
            let backend = RustFsClient::new().await;
            let cache = TieredCache::new(
                staging_dirs,
                mem_cache_size.as_deref(),
                disk_cache_size.as_deref(),
                backend.clone(),
                dlm.meta_client().clone(),
            )?;
            let router = DataRouter::new(dlm.clone(), backend, cache);
            let fs_engine = SqueezefsFilesystem::new(router, dlm);

            println!("Mounting Squeezefs at {:?}...", mountpoint);
            start_mount(mountpoint, fs_engine).await?;
        }
        Commands::Bench {
            path,
            threads,
            size,
        } => {
            run_benchmark(&path, threads, size).await?;
        }
    }

    Ok(())
}

async fn get_daemon_metrics() -> Option<HashMap<String, u64>> {
    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = squeezefs::dlm::MetaClient::new(&redis_url).ok()?;
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
    let baseline_metrics = get_daemon_metrics().await;

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

    // --- CLEANUP FILES ---
    println!("Cleaning up benchmark files...");
    for t_id in 0..threads {
        let file_path = path.join(format!("bench_big_{}.bin", t_id));
        let _ = fs::remove_file(file_path).await;
        for f_id in 0..small_files_count {
            let file_path = path.join(format!("bench_small_{}_{}.bin", t_id, f_id));
            let _ = fs::remove_file(file_path).await;
        }
    }

    // 2. Fetch post-benchmark metrics
    let post_metrics = get_daemon_metrics().await;

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
        let s = format!("{:>12.2} files/s", val);
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
