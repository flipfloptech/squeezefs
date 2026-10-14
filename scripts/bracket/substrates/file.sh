# Substrate `file`: sparse regular files on /build (ext4 on the venue's virtio
# disk /dev/vdc — a SPARSE image on the Mac's APFS). The daemon opens them
# O_DIRECT (charter §5.1); real block semantics, no fabric. The DATA chain is
# ONE device shared with everything else on /build (the zig caches, other
# agents' smokes): its `device bytes ÷ user bytes` is an UPPER bound and the
# record says so; the daemon's own device-write arms are exact. The METADATA
# volume is a dedicated loop device per set (`losetup --direct-io=on` over
# its image): its diskstats count only this daemon's metadata bytes, so the
# ledger's `meta_device_div_user` reads the leg's own traffic, never the
# shared disk's.
DATA_DEVICES_EXCLUSIVE=0
FILE_LOOPS=()

substrate_up() {   # bin nsets
  local nsets=$2 s v disk loop
  disk=$(backing_disk_of /build)
  META_DEV=(); DATA_DEVS_BY_SET=(); META_DISK_DEVS_BY_SET=(); DATA_DISK_DEVS_BY_SET=()
  DISK_DEVS=("$disk")
  for s in $(seq 0 $((nsets - 1))); do
    truncate -s "$META_SIZE" "$SCRATCH/set$s-meta.img"
    loop=$(losetup --direct-io=on -f --show "$SCRATCH/set$s-meta.img") || { log "losetup of set $s's metadata image failed"; exit 1; }
    FILE_LOOPS+=("$loop")
    META_DEV[$s]=$loop
    META_DISK_DEVS_BY_SET[$s]=$(basename "$loop")
    DISK_DEVS+=("$(basename "$loop")")
    local list=""
    for v in $(seq 0 $((DATA_VOLUMES - 1))); do
      truncate -s "$DATA_SIZE" "$SCRATCH/set$s-data$v.img"
      list="${list:+$list,}$SCRATCH/set$s-data$v.img"
    done
    DATA_DEVS_BY_SET[$s]=$list
    DATA_DISK_DEVS_BY_SET[$s]=$disk
  done
  SUBSTRATE_DESC="file-backed sparse volumes on /build ($(findmnt -no FSTYPE /build) on /dev/$disk, shared disk: the data diskstats ratio is an upper bound); metadata on a dedicated loop device per set (${META_DEV[*]}, exclusive diskstats)"
  log "substrate: $SUBSTRATE_DESC"
}

# The file volumes need nothing after a format or a mount.
substrate_after_format() { :; }
substrate_after_mount() { :; }

substrate_down() {
  local loop
  [ ${#FILE_LOOPS[@]} -gt 0 ] && for loop in "${FILE_LOOPS[@]}"; do
    losetup -d "$loop" || log "losetup -d $loop failed"
  done
  FILE_LOOPS=()
}
