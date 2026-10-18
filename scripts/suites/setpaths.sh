# Shared by the xfstests helpers (sourced): the SET behind a device string,
# and the ONE-DEVICE layout of that set.
#
# xfstests hands the helpers ONE block device per set (TEST_DEV, SCRATCH_DEV:
# loop devices; or a dm-flakey / dm-error / dm-log-writes target the test put
# over one) and its crash-consistency tests cut, error or log THAT device.
# A set's state spans a metadata volume, two data volumes and the staging
# cache, so every byte of it must sit on that one device: the helpers carve
# it with dm-linear into four slices of stable names,
#
#   /dev/mapper/sqz-<set>-meta    SUITE_META_MIB   the metadata volume
#   /dev/mapper/sqz-<set>-d0/-d1  SUITE_DATA_GIB   the data volumes
#   /dev/mapper/sqz-<set>-cache   SUITE_CACHE_GIB  an ext4, the staging root at $SET_DIR/cache
#
# created over whatever device the test hands over at mkfs/mount/fsck time
# (re-pointed if they exist) and removed at umount, so the test's own dm
# device can be removed by its cleanup. Before this (2026-10-17) the data
# volumes and the cache were files BESIDE the metadata image, outside every
# cut: a dm-flakey test lost fsync'ed data to a promotion whose data-volume
# write and staging reclaim survived while its metadata mapping was dropped.
#
# `suite_set_of <dev>` answers SET_DEV (the device string as handed over),
# SET_DIR (the directory beside the set's loop image: logs, the cache
# mountpoint) and SET_ID (`set-test`, `set-scratch`), plus the slice paths.
SUITE_META_MIB=${SUITE_META_MIB:-1024}
SUITE_DATA_GIB=${SUITE_DATA_GIB:-8}
SUITE_CACHE_GIB=${SUITE_CACHE_GIB:-4}

suite_set_of() {
  SET_DEV=$1
  local dev=$1 back
  case "$dev" in
    /dev/mapper/sqz-*) echo "$(basename "$0"): $dev is one of the set's own slices, not a set device" >&2; exit 1 ;;
    /dev/mapper/*|/dev/dm-*)
      # A device-mapper target over the loop (dm-flakey, dm-log-writes,
      # dm-delay): its table's first device. dm-error's table names none
      # (`0 N error`): the slices already standing over it name the set.
      local mm; mm=$(dmsetup table "$dev" 2>/dev/null | awk 'NR == 1 {print $4}')
      if [[ $mm =~ ^[0-9]+:[0-9]+$ ]]; then dev=/dev/$(basename "$(readlink -f "/sys/dev/block/$mm")")
      else
        local id; id=$(suite_set_id_over "$dev") || { echo "$(basename "$0"): $dev is a device-mapper target with no device in its table and no set's slices over it ($(dmsetup table "$dev" 2>&1 | head -1))" >&2; exit 1; }
        SET_DIR=$(cat "/run/squeezefs-suites/$id.dir"); SET_ID=$id; suite_slice_paths; return 0
      fi ;;
  esac
  back=$dev
  case "$dev" in
    /dev/loop*) back=$(losetup -O BACK-FILE -n "$dev" 2>/dev/null | head -1); [ -n "$back" ] || { echo "$(basename "$0"): $dev is not an attached loop device" >&2; exit 1; } ;;
    # Any other block device (a scsi_debug disk, a real one) backs no file:
    # the set's logs and cache mountpoint would land in /dev (devtmpfs).
    /dev/*) echo "$(basename "$0"): $dev is a block device with no backing file — the suites key a set by its metadata image's directory (a loop device over it, or a dm target over that loop)" >&2; exit 1 ;;
  esac
  SET_DIR=$(dirname "$back")
  SET_ID=$(basename "$SET_DIR")
  suite_slice_paths
  mkdir -p /run/squeezefs-suites && echo "$SET_DIR" > "/run/squeezefs-suites/$SET_ID.dir"
}

suite_slice_paths() {
  SLICE_META=/dev/mapper/sqz-$SET_ID-meta
  SLICE_D0=/dev/mapper/sqz-$SET_ID-d0
  SLICE_D1=/dev/mapper/sqz-$SET_ID-d1
  SLICE_CACHE=/dev/mapper/sqz-$SET_ID-cache
}

# The set whose `-meta` slice stands over `$1` (a dm device), by name.
suite_set_id_over() {
  local dev=$1 mm name
  mm=$(stat -c '%t:%T' "$(readlink -f "$dev")" 2>/dev/null) || return 1
  mm=$((16#${mm%%:*})):$((16#${mm##*:}))
  for name in $(dmsetup ls 2>/dev/null | awk '/^sqz-.*-meta\t/ || /^sqz-.*-meta / {print $1}'); do
    if [ "$(dmsetup table "$name" 2>/dev/null | awk '{print $4}')" = "$mm" ]; then name=${name#sqz-}; echo "${name%-meta}"; return 0; fi
  done
  return 1
}

# Bytes the one device must hold: the four slices.
suite_set_bytes() { echo $(( (SUITE_META_MIB << 20) + 2 * (SUITE_DATA_GIB << 30) + (SUITE_CACHE_GIB << 30) )); }

# The slices over `$1` (created, or re-pointed when they stand already).
suite_slices_up() {
  local dev=$1 ss=512 start=0 spec name len total
  total=$(blockdev --getsz "$dev") || { echo "$(basename "$0"): $dev: no size" >&2; exit 1; }
  [ "$total" -ge $(( $(suite_set_bytes) / ss )) ] || { echo "$(basename "$0"): $dev holds $((total * ss)) B; the set needs $(suite_set_bytes) B (SUITE_META_MIB + 2 × SUITE_DATA_GIB + SUITE_CACHE_GIB)" >&2; exit 1; }
  for spec in "meta:$((SUITE_META_MIB << 20))" "d0:$((SUITE_DATA_GIB << 30))" "d1:$((SUITE_DATA_GIB << 30))" "cache:$((SUITE_CACHE_GIB << 30))"; do
    name=sqz-$SET_ID-${spec%%:*}; len=$(( ${spec#*:} / ss ))
    if dmsetup info "$name" >/dev/null 2>&1; then
      dmsetup suspend "$name" && dmsetup load "$name" --table "0 $len linear $dev $start" && dmsetup resume "$name"
    else
      dmsetup create "$name" --table "0 $len linear $dev $start"
    fi || { echo "$(basename "$0"): slice $name over $dev failed" >&2; exit 1; }
    start=$((start + len))
  done
}

# The cache slice's ext4 at $SET_DIR/cache (the staging root).
suite_cache_mount() {
  mkdir -p "$SET_DIR/cache"
  mountpoint -q "$SET_DIR/cache" || mount -t ext4 -o noatime "$SLICE_CACHE" "$SET_DIR/cache" || { echo "$(basename "$0"): the cache slice $SLICE_CACHE does not mount at $SET_DIR/cache" >&2; exit 1; }
  chmod 0700 "$SET_DIR/cache"
}

suite_cache_umount() {
  # By device: a previous life's cache may stand at another set directory.
  local t
  for t in $(findmnt -n -o TARGET -S "$SLICE_CACHE" 2>/dev/null); do umount "$t" || true; done
  return 0
}

# Everything down: the cache unmounted, the four slices removed (a holder
# still closing gets dmsetup's retries), so the test's own dm device above
# them can go.
suite_slices_down() {
  suite_cache_umount
  local s
  for s in cache d1 d0 meta; do dmsetup info "sqz-$SET_ID-$s" >/dev/null 2>&1 && dmsetup remove --retry "sqz-$SET_ID-$s" 2>/dev/null; done
  return 0
}
