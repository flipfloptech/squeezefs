#!/usr/bin/env python3
import os
import sys
import time
import argparse
import hashlib
import urllib.request
import urllib.error

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
        with urllib.request.urlopen(req) as response:
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
        with urllib.request.urlopen(req) as response:
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

def main():
    parser = argparse.ArgumentParser(description="Long running validation test for squeezefs mounts.")
    parser.add_argument("--dir", required=True, help="Target directory (usually the squeezefs mount point)")
    parser.add_argument("--limit", type=str, choices=["small", "medium", "large"], default="large",
                        help="Size limit of downloads (small <= 38MB, medium <= 370MB, large <= 2.6GB)")
    parser.add_argument("--loops", type=int, default=0, help="Number of loops to run (0 for infinite)")
    
    args = parser.parse_args()
    
    if not os.path.isdir(args.dir):
        print(f"Error: {args.dir} is not a valid directory.")
        sys.exit(1)
        
    # Filter targets based on limit
    run_targets = []
    for t in TARGETS:
        if args.limit == "small" and "Small" not in t["name"]:
            continue
        if args.limit == "medium" and "Large" in t["name"]:
            continue
        run_targets.append(t)
        
    print(f"Starting long validation test on mount: {args.dir}")
    print(f"Running with limit: {args.limit} ({len(run_targets)} targets)")
    
    loop_count = 0
    try:
        while True:
            loop_count += 1
            print(f"\n========================================")
            print(f"Starting Iteration {loop_count}")
            print(f"========================================")
            
            for target in run_targets:
                print(f"\nRunning test for target: {target['name']}")
                success = download_and_verify(target, args.dir)
                if not success:
                    print("🔴 Target validation FAILED! Halting execution.")
                    sys.exit(2)
                print("🟢 Target test complete.")
                
            if args.loops > 0 and loop_count >= args.loops:
                print(f"\nCompleted {loop_count} iterations successfully.")
                break
                
            time.sleep(1)
    except KeyboardInterrupt:
        print("\nTest cancelled by user. Exiting cleanly.")
        sys.exit(0)

if __name__ == "__main__":
    main()
