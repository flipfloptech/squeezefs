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
    print(f"Fetching expected SHA-256 for {target['name']}...")
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
        print(f"Error fetching expected hash: {e}")
        return None
    return None

def download_and_verify(target, dest_dir):
    dest_path = os.path.join(dest_dir, target["filename"])
    expected_hash = fetch_expected_hash(target)
    if not expected_hash:
        print("Could not obtain expected hash. Skipping this target.")
        return False

    print(f"Expected Hash: {expected_hash}")
    
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
                    
                    elapsed = time.time() - start_time
                    speed = (downloaded_bytes / (1024 * 1024)) / elapsed if elapsed > 0 else 0
                    
                    if total_size > 0:
                        pct = (downloaded_bytes / total_size) * 100
                        print(f"\rProgress: {pct:.2f}% ({downloaded_bytes / (1024*1024):.1f}/{total_size / (1024*1024):.1f} MB) | Speed: {speed:.2f} MB/s", end="", flush=True)
                    else:
                        print(f"\rDownloaded: {downloaded_bytes / (1024*1024):.1f} MB | Speed: {speed:.2f} MB/s", end="", flush=True)
        print("\nDownload complete. Verifying signature...")
        actual_hash = sha256.hexdigest()
        print(f"Actual Hash:   {actual_hash}")
        
        if actual_hash == expected_hash:
            print("🟢 Verification SUCCESS!")
            return True
        else:
            print("🔴 Verification FAILURE! Hashes do not match.")
            return False
            
    except Exception as e:
        print(f"\nError occurred during download/verification: {e}")
        return False
    finally:
        # Clean up the file
        if os.path.exists(dest_path):
            print(f"Cleaning up {dest_path}...")
            try:
                os.remove(dest_path)
                print("Cleaned up successfully.")
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

def create_tree_recursive(base_dir, depth, max_depth, breadth, files_per_dir, file_size_range, file_map):
    if depth > max_depth:
        return
    
    os.makedirs(base_dir, exist_ok=True)
    
    # Create files in this directory
    for i in range(files_per_dir):
        filename = f"file_{i}.dat"
        filepath = os.path.join(base_dir, filename)
        # Deterministic size based on filename and index to keep it consistent
        size = file_size_range[0] + (i * 37 + depth * 13) % (file_size_range[1] - file_size_range[0] + 1)
        content = generate_file_content(filepath, i, size)
        
        with open(filepath, "wb") as f:
            f.write(content)
            
        file_map[filepath] = hashlib.sha256(content).hexdigest()
        
    # Create subdirectories
    for b in range(breadth):
        subdir_name = f"dir_{b}"
        subdir_path = os.path.join(base_dir, subdir_name)
        create_tree_recursive(subdir_path, depth + 1, max_depth, breadth, files_per_dir, file_size_range, file_map)

def verify_and_clean_tree(base_dir, file_map):
    # Verify hashes
    print(f"Verifying directory tree files in {base_dir}...")
    verified_count = 0
    errors = 0
    for filepath, expected_hash in file_map.items():
        if not os.path.exists(filepath):
            print(f"Error: Expected file {filepath} does not exist!")
            errors += 1
            continue
        try:
            with open(filepath, "rb") as f:
                data = f.read()
            actual_hash = hashlib.sha256(data).hexdigest()
            if actual_hash != expected_hash:
                print(f"Error: Hash mismatch for file {filepath}!")
                errors += 1
            else:
                verified_count += 1
        except Exception as e:
            print(f"Error reading/verifying {filepath}: {e}")
            errors += 1
            
    print(f"Verification complete: {verified_count} verified, {errors} errors.")
    
    # Cleanup tree using recursive delete
    print("Cleaning up directory tree...")
    try:
        shutil.rmtree(base_dir)
        print("Directory tree cleaned up successfully.")
    except Exception as e:
        print(f"Error cleaning up directory tree: {e}")
        errors += 1
        
    return errors == 0

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

def remount_squeezefs(squeezefs_bin, meta_uri, mount_dir, disk_cache_paths, log_file):
    print("Dismounting Squeezefs...")
    # Lazy unmount
    subprocess.run(["fusermount", "-z", "-u", mount_dir])
    time.sleep(2)
    
    # Forcefully terminate any lingering daemon process for this mount point
    kill_old_squeezefs_daemon(mount_dir)
    
    print("Remounting Squeezefs...")
    cmd = [
        squeezefs_bin,
        "mount",
        meta_uri,
        mount_dir,
        "--daemon",
        "--disk-cache-paths",
        disk_cache_paths,
        "--log-file",
        log_file
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

def main():
    parser = argparse.ArgumentParser(description="Long running validation test for squeezefs mounts.")
    parser.add_argument("--dir", required=True, help="Target directory (usually the squeezefs mount point)")
    parser.add_argument("--limit", type=str, choices=["none", "small", "medium", "large"], default="large",
                        help="Size limit of downloads (small <= 38MB, medium <= 370MB, large <= 2.6GB)")
    parser.add_argument("--loops", type=int, default=0, help="Number of loops to run (0 for infinite)")
    
    # Remount options
    parser.add_argument("--squeezefs-bin", help="Squeezefs binary path for remount testing")
    parser.add_argument("--meta-uri", help="Metadata volume URI for remount testing")
    parser.add_argument("--disk-cache-paths", help="Disk cache paths for remount testing")
    parser.add_argument("--log-file", help="Log file path for remount testing")
    parser.add_argument("--remount-frequency", type=int, default=1, help="Frequency of remounts (every N loops)")
    
    # Tree configuration
    parser.add_argument("--tree-depth", type=int, default=4, help="Recursive tree depth")
    parser.add_argument("--tree-breadth", type=int, default=4, help="Directory breadth at each level")
    parser.add_argument("--tree-files", type=int, default=5, help="Number of files per directory")
    
    args = parser.parse_args()
    
    if not os.path.isdir(args.dir):
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
            print(f"Generating recursive directory tree at {tree_base}...")
            create_tree_recursive(
                base_dir=tree_base,
                depth=1,
                max_depth=args.tree_depth,
                breadth=args.tree_breadth,
                files_per_dir=args.tree_files,
                file_size_range=(100, 15000),  # 100 bytes to 15KB
                file_map=file_map
            )
            print(f"Successfully generated {len(file_map)} files inside directory tree.")
            
            # Verify and delete
            if not verify_and_clean_tree(tree_base, file_map):
                print("🔴 Directory tree verification or cleanup FAILED!")
                sys.exit(3)
            print("🟢 Directory tree phase complete.")
            
            # Phase 2: Download & Hash Verification
            for target in run_targets:
                print(f"\nRunning download test for target: {target['name']}")
                success = download_and_verify(target, args.dir)
                if not success:
                    print("🔴 Target download validation FAILED! Halting execution.")
                    sys.exit(2)
                print("🟢 Target download test complete.")
                
            # Phase 3: Dismount & Remount scenario
            can_remount = all([args.squeezefs_bin, args.meta_uri, args.disk_cache_paths, args.log_file])
            if can_remount and loop_count % args.remount_frequency == 0:
                print(f"\nTriggering periodic remount scenario (iteration {loop_count})...")
                if not remount_squeezefs(
                    squeezefs_bin=args.squeezefs_bin,
                    meta_uri=args.meta_uri,
                    mount_dir=args.dir,
                    disk_cache_paths=args.disk_cache_paths,
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
