# Substrate `file`: sparse regular files on /build (ext4 on the venue's virtio
# disk /dev/vdc — a SPARSE image on the Mac's APFS). The daemon opens them
# O_DIRECT (charter §5.1); real block semantics, no fabric. The diskstats
# chain is ONE device shared with everything else on /build (the zig caches,
# other agents' smokes): its `device bytes ÷ user bytes` is an UPPER bound
# and the record says so; the daemon's own device-write arms are exact.
DATA_DEVICES_EXCLUSIVE=0

substrate_up() {   # bin nsets
  local nsets=$2 s v disk
  disk=$(backing_disk_of /build)
  META_DEV=(); DATA_DEVS_BY_SET=(); META_DISK_DEVS_BY_SET=(); DATA_DISK_DEVS_BY_SET=()
  for s in $(seq 0 $((nsets - 1))); do
    truncate -s "$META_SIZE" "$SCRATCH/set$s-meta.img"
    META_DEV[$s]="$SCRATCH/set$s-meta.img"
    local list=""
    for v in $(seq 0 $((DATA_VOLUMES - 1))); do
      truncate -s "$DATA_SIZE" "$SCRATCH/set$s-data$v.img"
      list="${list:+$list,}$SCRATCH/set$s-data$v.img"
    done
    DATA_DEVS_BY_SET[$s]=$list
    META_DISK_DEVS_BY_SET[$s]=$disk
    DATA_DISK_DEVS_BY_SET[$s]=$disk
  done
  DISK_DEVS=("$disk")
  SUBSTRATE_DESC="file-backed sparse volumes on /build ($(findmnt -no FSTYPE /build) on /dev/$disk, shared disk: diskstats ratio is an upper bound)"
}

substrate_down() { :; }
