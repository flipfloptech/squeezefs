# Shared by the xfstests helpers (sourced): the SET behind a device string.
# A set's metadata volume is a plain file or a loop device over one; the
# data volumes `d0.img`/`d1.img`, the staging root `cache/` and the daemon
# logs live BESIDE the metadata image. `suite_set_of <dev>` answers:
#   SET_DEV   the device string as the daemon sees it (sqmeta://$SET_DEV)
#   SET_DIR   the directory holding the set's files
# A device-mapper target over the loop device (dm-flakey, dm-log-writes,
# dm-delay: the crash-consistency tests put one over SCRATCH_DEV) is the
# same set: its table's first device is the loop, the loop's backing file
# names the set — else the set would be "in /dev/mapper" (two 8 GiB data
# images and a cache written into devtmpfs, on a venue with dm).
suite_set_of() {
  SET_DEV=$1
  local dev=$1 back
  case "$dev" in
    /dev/mapper/*|/dev/dm-*)
      local mm; mm=$(dmsetup table "$dev" 2>/dev/null | awk 'NR == 1 {print $4}')
      [[ $mm =~ ^[0-9]+:[0-9]+$ ]] || { echo "$(basename "$0"): $dev is a device-mapper target with no device in its table ($(dmsetup table "$dev" 2>&1 | head -1))" >&2; exit 1; }
      dev=/dev/$(basename "$(readlink -f "/sys/dev/block/$mm")") ;;
  esac
  back=$dev
  case "$dev" in
    /dev/loop*) back=$(losetup -O BACK-FILE -n "$dev" 2>/dev/null | head -1); [ -n "$back" ] || { echo "$(basename "$0"): $dev is not an attached loop device" >&2; exit 1; } ;;
  esac
  SET_DIR=$(dirname "$back")
}
