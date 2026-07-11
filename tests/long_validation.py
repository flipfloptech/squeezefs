#!/usr/bin/env python3
import os
import sys
import time
import argparse
import hashlib
import urllib.request
import urllib.error
import subprocess
import shutil
import threading
import concurrent.futures

# Predefined list of validation targets with direct links and hash reference sites
TARGETS = [
    {
        "name": "rust-std-1.78.0-x86_64 (Small, ~38 MB)",
        "url": "https://static.rust-lang.org/dist/rust-std-1.78.0-x86_64-unknown-linux-gnu.tar.xz",
        "hash_type": "rust",
        "hash_url": "https://static.rust-lang.org/dist/rust-std-1.78.0-x86_64-unknown-linux-gnu.tar.xz.sha256",
        "filename": "rust-std-1.78.0-x86_64-unknown-linux-gnu.tar.xz"
    },
    {
        "name": "rustc-1.78.0-x86_64 (Medium, ~156 MB)",
        "url": "https://static.rust-lang.org/dist/rustc-1.78.0-x86_64-unknown-linux-gnu.tar.xz",
        "hash_type": "rust",
        "hash_url": "https://static.rust-lang.org/dist/rustc-1.78.0-x86_64-unknown-linux-gnu.tar.xz.sha256",
        "filename": "rustc-1.78.0-x86_64-unknown-linux-gnu.tar.xz"
    },
    {
        "name": "rustc-1.78.0-src (Medium, ~370 MB)",
        "url": "https://static.rust-lang.org/dist/rustc-1.78.0-src.tar.gz",
        "hash_type": "rust",
        "hash_url": "https://static.rust-lang.org/dist/rustc-1.78.0-src.tar.gz.sha256",
        "filename": "rustc-1.78.0-src.tar.gz"
    },
    {
        "name": "ubuntu-24.04-live-server (Large, ~2.6 GB)",
        "url": "https://releases.ubuntu.com/24.04/ubuntu-24.04-live-server-amd64.iso",
        "hash_type": "ubuntu",
        "hash_url": "https://releases.ubuntu.com/24.04/SHA256SUMS",
        "filename": "ubuntu-24.04-live-server-amd64.iso"
    }
]

def fetch_expected_hash(target):
    print(f"[{target['name']}] Fetching expected SHA-256...")
    req = urllib.request.Request(
        target["hash_url"],
        headers={"User-Agent": "Mozilla/5.0"}
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as response:
            content = response.read().decode("utf-8")
            if target["hash_type"] == "rust":
                return content.strip()[:64]
            elif target["hash_type"] == "ubuntu":
                for line in content.splitlines():
                    if target["filename"] in line:
                        return line.split()[0].strip()
    except Exception as e:
        print(f"[{target['name']}] Error fetching expected hash: {e}")
        return None
    return None

def download_and_verify(target, dest_dir, verbose=True):
    dest_path = os.path.join(dest_dir, target["filename"])
    expected_hash = fetch_expected_hash(target)
    if not expected_hash:
        print(f"[{target['name']}] Could not obtain expected hash. Skipping this target.")
        return False

    if verbose:
        print(f"[{target['name']}] Expected Hash: {expected_hash}")
    else:
        print(f"[{target['name']}] Started download and verification...")

    sha256 = hashlib.sha256()
    start_time = time.time()
    downloaded_bytes = 0
    
    req = urllib.request.Request(
        target["url"],
        headers={"User-Agent": "Mozilla/5.0"}
    )
    
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            total_size = int(response.info().get("Content-Length", 0))
            chunk_size = 1024 * 1024  # 1MB chunks
            
            with open(dest_path, "wb") as f:
                while True:
                    chunk = response.read(chunk_size)
                    if not chunk:
                        break
                    f.write(chunk)
                    sha256.update(chunk)
                    downloaded_bytes += len(chunk)
                    
                    if verbose:
                        elapsed = time.time() - start_time
                        speed = (downloaded_bytes / (1024 * 1024)) / elapsed if elapsed > 0 else 0
                        if total_size > 0:
                            pct = (downloaded_bytes / total_size) * 100
                            print(f"\rProgress [{target['name']}]: {pct:.2f}% ({downloaded_bytes / (1024*1024):.1f}/{total_size / (1024*1024):.1f} MB) | Speed: {speed:.2f} MB/s", end="", flush=True)
                        else:
                            print(f"\rDownloaded [{target['name']}]: {downloaded_bytes / (1024*1024):.1f} MB | Speed: {speed:.2f} MB/s", end="", flush=True)
        if verbose:
            print(f"\n[{target['name']}] Download complete. Verifying signature...")
        actual_hash = sha256.hexdigest()
        
        if actual_hash == expected_hash:
            print(f"🟢 [{target['name']}] Verification SUCCESS!")
            return True
        else:
            print(f"🔴 [{target['name']}] Verification FAILURE! Hashes do not match.")
            return False
            
    except Exception as e:
        print(f"\n[{target['name']}] Error occurred during download/verification: {e}")
        return False
    finally:
        # Clean up the file
        if os.path.exists(dest_path):
            if verbose:
                print(f"Cleaning up {dest_path}...")
            try:
                os.remove(dest_path)
            except Exception as e:
                print(f"Error deleting file: {e}")

def generate_file_content(path, index, size):
    header = f"path={path}|index={index}|size={size}\n"
    header_bytes = header.encode('utf-8')
    if len(header_bytes) >= size:
        return header_bytes[:size]
    padding_len = size - len(header_bytes)
    pattern = b"SqueezeFS_Validation_Pattern_For_Stress_Testing_POSIX_Compliance_"
    padding = (pattern * (padding_len // len(pattern) + 1))[:padding_len]
    return header_bytes + padding

def generate_dir_files(dir_path, depth, files_per_dir, file_size_range, file_map, lock):
    os.makedirs(dir_path, exist_ok=True)
    local_map = {}
    for i in range(files_per_dir):
        filename = f"file_{i}.dat"
        filepath = os.path.join(dir_path, filename)
        size = file_size_range[0] + (i * 37 + depth * 13) % (file_size_range[1] - file_size_range[0] + 1)
        content = generate_file_content(filepath, i, size)
        
        with open(filepath, "wb") as f:
            f.write(content)
            
        local_map[filepath] = hashlib.sha256(content).hexdigest()
    with lock:
        file_map.update(local_map)

def collect_dirs_recursive(base_dir, depth, max_depth, breadth, dirs_to_create):
    if depth > max_depth:
        return
    dirs_to_create.append((base_dir, depth))
    for b in range(breadth):
        subdir_name = f"dir_{b}"
        subdir_path = os.path.join(base_dir, subdir_name)
        collect_dirs_recursive(subdir_path, depth + 1, max_depth, breadth, dirs_to_create)

def verify_single_file(filepath, expected_hash, results_lock, counter_dict):
    if not os.path.exists(filepath):
        print(f"Error: Expected file {filepath} does not exist!")
        with results_lock:
            counter_dict["errors"] += 1
        return
    try:
        with open(filepath, "rb") as f:
            data = f.read()
        actual_hash = hashlib.sha256(data).hexdigest()
        with results_lock:
            if actual_hash != expected_hash:
                print(f"Error: Hash mismatch for file {filepath}!")
                counter_dict["errors"] += 1
            else:
                counter_dict["verified"] += 1
    except Exception as e:
        print(f"Error reading/verifying {filepath}: {e}")
        with results_lock:
            counter_dict["errors"] += 1

def verify_and_clean_tree(base_dir, file_map, num_threads):
    print(f"Verifying directory tree files in {base_dir} (using {num_threads} threads)...")
    results_lock = threading.Lock()
    counter_dict = {"verified": 0, "errors": 0}
    
    with concurrent.futures.ThreadPoolExecutor(max_workers=num_threads) as executor:
        futures = [
            executor.submit(verify_single_file, filepath, expected_hash, results_lock, counter_dict)
            for filepath, expected_hash in file_map.items()
        ]
        concurrent.futures.wait(futures)
        
    print(f"Verification complete: {counter_dict['verified']} verified, {counter_dict['errors']} errors.")
    
    # Cleanup tree using recursive delete
    print("Cleaning up directory tree...")
    try:
        shutil.rmtree(base_dir)
        print("Directory tree cleaned up successfully.")
    except Exception as e:
        print(f"Error cleaning up directory tree: {e}")
        counter_dict["errors"] += 1
        
    return counter_dict["errors"] == 0

def kill_old_squeezefs_daemon(mount_dir):
    try:
        # Find the process ID associated with this mount directory
        res = subprocess.run(
            ["pgrep", "-f", f"squeezefs mount.*{mount_dir}"],
            capture_output=True,
            text=True
        )
        if res.returncode == 0:
            for line in res.stdout.splitlines():
                pid = line.strip()
                if pid:
                    print(f"Killing old squeezefs process {pid} for mount {mount_dir}...")
                    subprocess.run(["kill", "-9", pid])
                    time.sleep(1)
    except Exception as e:
        print(f"Error checking/killing squeezefs process: {e}")

def remount_squeezefs(squeezefs_bin, meta_uri, mount_dir, log_file):
    print("Dismounting Squeezefs...")
    # Lazy unmount
    subprocess.run(["fusermount", "-z", "-u", mount_dir])
    time.sleep(2)
    
    # Forcefully terminate any lingering daemon process for this mount point
    kill_old_squeezefs_daemon(mount_dir)
    
    # Cache-path policy: staging dirs come from the format config; mount
    # rejects --disk-cache-paths.
    print("Remounting Squeezefs...")
    cmd = [
        squeezefs_bin,
        "mount",
        meta_uri,
        mount_dir,
        "--daemon",
        "--log-file",
        log_file,
        "--allow-others"
    ]
    print(f"Running command: {' '.join(cmd)}")
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        print(f"Remount failed with code {res.returncode}")
        print(f"Stdout: {res.stdout}")
        print(f"Stderr: {res.stderr}")
        return False
    
    time.sleep(2)
    return True

def run_crash_soak(rounds):
    """Nightly kill-9 remount soak (design-wal-crash-consistency §4.7b):
    the 500-round variant of tests/crash_kill_tests.rs. Runs against
    file-backed temp volumes — no mount required."""
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    env = dict(os.environ, SQUEEZEFS_CRASH_ROUNDS=str(rounds))
    print(f"[crash-soak] kill-9 remount soak: {rounds} rounds")
    result = subprocess.run(
        ["cargo", "test", "--release", "--test", "crash_kill_tests", "--",
         "--exact", "test_kill9_remount_soak", "--test-threads=1", "--nocapture"],
        cwd=repo_root, env=env,
    )
    if result.returncode != 0:
        print("[crash-soak] FAILED")
        sys.exit(result.returncode)
    print("[crash-soak] PASSED")


# Nightly builder-built mount-time cases (design-cow-kv-metadata §8 row 5,
# PR K7): 10M- and 100M-ino kv::builder images, cold-mounted (fdatasync'd
# then fadvise(DONTNEED)) with the §3 bound asserted in the Rust test
# (≤ 2 s hard; ≤ ~300 ms typical target for 100M). The 1M-ino case runs in
# the serial cargo gate; these two are nightly because the 100M build
# writes a ~25 GiB image and holds a multi-GB description in RAM.
#
# Standalone invocation (what this wrapper runs for you):
#   cargo test --release --test kv_scale_tests -- --ignored \
#       --exact nightly_mount_time_10m_ino --test-threads=1 --nocapture
#   cargo test --release --test kv_scale_tests -- --ignored \
#       --exact nightly_mount_time_100m_ino --test-threads=1 --nocapture
MOUNT_SCALE_TESTS = {
    "10m": ["nightly_mount_time_10m_ino"],
    "100m": ["nightly_mount_time_100m_ino"],
    "both": ["nightly_mount_time_10m_ino", "nightly_mount_time_100m_ino"],
}


def run_mount_scale(which):
    """Run the 10M / 100M-ino nightly mount-time cases (PR K7 §8 row 5)."""
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for test in MOUNT_SCALE_TESTS[which]:
        print(f"[mount-scale] {test} (builder image + cold mount; "
              f"images land in target/tmp and are removed afterwards)")
        result = subprocess.run(
            ["cargo", "test", "--release", "--test", "kv_scale_tests", "--",
             "--ignored", "--exact", test, "--test-threads=1", "--nocapture"],
            cwd=repo_root,
        )
        if result.returncode != 0:
            print(f"[mount-scale] {test} FAILED")
            sys.exit(result.returncode)
        print(f"[mount-scale] {test} PASSED")


def main():
    parser = argparse.ArgumentParser(description="Long running validation test for squeezefs mounts.")
    parser.add_argument("--dir", help="Target directory (usually the squeezefs mount point)")
    parser.add_argument("--limit", type=str, choices=["none", "small", "medium", "large"], default="large",
                        help="Size limit of downloads (small <= 38MB, medium <= 370MB, large <= 2.6GB)")
    parser.add_argument("--loops", type=int, default=0, help="Number of loops to run (0 for infinite)")
    parser.add_argument("--threads", type=int, default=4, help="Number of worker threads to run validation tasks")
    
    # Remount options (cache paths come from the format config; mount
    # rejects --disk-cache-paths)
    parser.add_argument("--squeezefs-bin", help="Squeezefs binary path for remount testing")
    parser.add_argument("--meta-uri", help="Metadata volume URI for remount testing")
    parser.add_argument("--log-file", help="Log file path for remount testing")
    parser.add_argument("--remount-frequency", type=int, default=1, help="Frequency of remounts (every N loops)")
    
    # Tree configuration
    parser.add_argument("--tree-depth", type=int, default=4, help="Recursive tree depth")
    parser.add_argument("--tree-breadth", type=int, default=4, help="Directory breadth at each level")
    parser.add_argument("--tree-files", type=int, default=5, help="Number of files per directory")

    # Crash soak (no mount required; runs the kill-9 harness at soak scale)
    parser.add_argument("--crash-soak", type=int, default=0, metavar="N",
                        help="Run the kill-9 remount soak for N rounds (nightly: 500) and exit")

    # Builder-built mount-time scale cases (no mount required; PR K7 §8
    # row 5 nightly halves — the 1M case lives in the serial cargo gate)
    parser.add_argument("--mount-scale", choices=sorted(MOUNT_SCALE_TESTS),
                        help="Run the 10M/100M-ino builder-built cold-mount "
                             "cases (nightly) and exit")

    args = parser.parse_args()

    if args.crash_soak > 0:
        run_crash_soak(args.crash_soak)
        return

    if args.mount_scale:
        run_mount_scale(args.mount_scale)
        return

    if not args.dir or not os.path.isdir(args.dir):
        print(f"Error: {args.dir} is not a valid directory.")
        sys.exit(1)
        
    # Filter targets based on limit
    run_targets = []
    if args.limit != "none":
        for t in TARGETS:
            if args.limit == "small" and "Small" not in t["name"]:
                continue
            if args.limit == "medium" and "Large" in t["name"]:
                continue
            run_targets.append(t)
        
    print(f"Starting long validation test on mount: {args.dir}")
    print(f"Running with download limit: {args.limit} ({len(run_targets)} targets)")
    print(f"Tree structure: depth={args.tree_depth}, breadth={args.tree_breadth}, files/dir={args.tree_files}")
    print(f"Running with {args.threads} worker threads")
    
    loop_count = 0
    try:
        while True:
            loop_count += 1
            print(f"\n========================================")
            print(f"Starting Iteration {loop_count} | Local Time: {time.strftime('%Y-%m-%d %H:%M:%S')}")
            print(f"========================================")
            
            # Phase 1: Directory Tree Creation & Verification
            tree_base = os.path.join(args.dir, f"stress_tree_loop_{loop_count}")
            file_map = {}
            print(f"Generating directory tree at {tree_base}...")
            
            dirs_to_create = []
            collect_dirs_recursive(tree_base, 1, args.tree_depth, args.tree_breadth, dirs_to_create)
            
            map_lock = threading.Lock()
            with concurrent.futures.ThreadPoolExecutor(max_workers=args.threads) as executor:
                futures = [
                    executor.submit(
                        generate_dir_files,
                        dir_path,
                        depth,
                        args.tree_files,
                        (100, 15000),
                        file_map,
                        map_lock
                    )
                    for dir_path, depth in dirs_to_create
                ]
                concurrent.futures.wait(futures)
                
            print(f"Successfully generated {len(file_map)} files inside directory tree.")
            
            # Verify and delete
            if not verify_and_clean_tree(tree_base, file_map, args.threads):
                print("🔴 Directory tree verification or cleanup FAILED!")
                sys.exit(3)
            print("🟢 Directory tree phase complete.")
            
            # Phase 2: Download & Hash Verification (Parallelized if threads > 1)
            print(f"Starting parallel download validation for {len(run_targets)} targets...")
            is_parallel = args.threads > 1
            
            if is_parallel:
                with concurrent.futures.ThreadPoolExecutor(max_workers=min(args.threads, len(run_targets))) as executor:
                    futures = {
                        executor.submit(download_and_verify, target, args.dir, verbose=False): target
                        for target in run_targets
                    }
                    for future in concurrent.futures.as_completed(futures):
                        target = futures[future]
                        try:
                            success = future.result()
                            if not success:
                                print(f"🔴 Target download validation FAILED for {target['name']}! Halting execution.")
                                sys.exit(2)
                            print(f"🟢 Target download test complete: {target['name']}")
                        except Exception as e:
                            print(f"🔴 Target download exception for {target['name']}: {e}")
                            sys.exit(2)
            else:
                for target in run_targets:
                    print(f"\nRunning download test for target: {target['name']}")
                    success = download_and_verify(target, args.dir, verbose=True)
                    if not success:
                        print("🔴 Target download validation FAILED! Halting execution.")
                        sys.exit(2)
                    print("🟢 Target download test complete.")
                
            # Phase 3: Dismount & Remount scenario
            can_remount = all([args.squeezefs_bin, args.meta_uri, args.log_file])
            if can_remount and loop_count % args.remount_frequency == 0:
                print(f"\nTriggering periodic remount scenario (iteration {loop_count})...")
                if not remount_squeezefs(
                    squeezefs_bin=args.squeezefs_bin,
                    meta_uri=args.meta_uri,
                    mount_dir=args.dir,
                    log_file=args.log_file
                ):
                    print("🔴 Remount scenario FAILED! Halting execution.")
                    sys.exit(4)
                print("🟢 Remount scenario success.")
                
            if args.loops > 0 and loop_count >= args.loops:
                print(f"\nCompleted {loop_count} iterations successfully.")
                break
                
            time.sleep(1)
    except KeyboardInterrupt:
        print("\nTest cancelled by user. Exiting cleanly.")
        sys.exit(0)

if __name__ == "__main__":
    main()
