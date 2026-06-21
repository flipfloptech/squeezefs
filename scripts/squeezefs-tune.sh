#!/bin/bash
set -e

# Squeezefs HPC Client Node Auto-Tuning Script
# Must be run as root to apply changes.

echo "=== Squeezefs Client Node Auto-Tuning Script ==="

if [ "$EUID" -ne 0 ]; then
  echo "WARNING: This script is not running as root. It will check settings but cannot apply changes."
  echo "To apply changes, please run: sudo \$0"
  echo ""
fi

# 1. Tuning Virtual Memory (dirty page ratios) to buffer write sequences in RAM
echo "Checking vm.dirty_ratio and vm.dirty_background_ratio..."
current_dirty_ratio=$(sysctl -n vm.dirty_ratio)
current_dirty_bg_ratio=$(sysctl -n vm.dirty_background_ratio)
echo "Current dirty_ratio = $current_dirty_ratio, dirty_background_ratio = $current_dirty_bg_ratio"

if [ "$EUID" -eq 0 ]; then
  echo "Applying VM dirty ratios (ratio=40, background_ratio=10)..."
  sysctl -w vm.dirty_ratio=40 >/dev/null
  sysctl -w vm.dirty_background_ratio=10 >/dev/null
  echo "Optimized VM dirty ratios applied."
fi

# 2. Tuning Network socket buffer limits for high-performance networks (RoCE / InfiniBand)
echo ""
echo "Checking net.core socket read/write buffer maximums..."
current_rmem_max=$(sysctl -n net.core.rmem_max)
current_wmem_max=$(sysctl -n net.core.wmem_max)
echo "Current rmem_max = $current_rmem_max bytes, wmem_max = $current_wmem_max bytes"

if [ "$EUID" -eq 0 ]; then
  echo "Optimizing net.core socket buffers (rmem_max=67108864, wmem_max=67108864)..."
  sysctl -w net.core.rmem_max=67108864 >/dev/null
  sysctl -w net.core.wmem_max=67108864 >/dev/null
  echo "Optimized net.core socket buffers applied."
fi

# 3. Tuning max open file descriptors limit (ulimit -n)
echo ""
echo "Checking max open file descriptors limit..."
current_ulimit=$(ulimit -n)
echo "Current ulimit -n = $current_ulimit"

if [ "$current_ulimit" -lt 65536 ]; then
  if [ "$EUID" -eq 0 ]; then
    echo "Setting ulimit -n to 65536..."
    ulimit -n 65536
    echo "Ulimit increased for this shell session."
    echo "To make it permanent, please add the following to /etc/security/limits.conf:"
    echo "* soft nofile 65536"
    echo "* hard nofile 65536"
  fi
else
  echo "File descriptor limit is already optimal ($current_ulimit)."
fi

# 4. Checking FUSE settings
echo ""
echo "Checking FUSE max_read and max_write configurations..."
if [ -d /sys/fs/fuse/connections ]; then
  for conn in /sys/fs/fuse/connections/*; do
    if [ -d "$conn" ]; then
      conn_id=$(basename "$conn")
      if [ -f "$conn/max_background" ]; then
        curr_bg=$(cat "$conn/max_background")
        curr_cong=$(cat "$conn/congestion_threshold")
        echo "FUSE Connection $conn_id: max_background = $curr_bg, congestion_threshold = $curr_cong"
        if [ "$EUID" -eq 0 ]; then
          echo "Tuning connection $conn_id parameters..."
          echo 64 > "$conn/max_background" 2>/dev/null || true
          echo 48 > "$conn/congestion_threshold" 2>/dev/null || true
        fi
      fi
    fi
  done
else
  echo "FUSE driver sysfs connections dir not found. Ensure FUSE is loaded/mounted."
fi

echo ""
echo "=== Tuning check and configuration completed ==="
