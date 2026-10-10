# Shared by the xfstests helpers (sourced): the SET behind a device string.
# A set's metadata volume is a plain file or a loop device over one; the
# data volumes `d0.img`/`d1.img`, the staging root `cache/` and the daemon
# logs live BESIDE the metadata image. `suite_set_of <dev>` answers:
#   SET_DEV   the device string as the daemon sees it (sqmeta://$SET_DEV)
#   SET_DIR   the directory holding the set's files
suite_set_of() {
  SET_DEV=$1
  local back=$1
  case "$1" in
    /dev/loop*) back=$(losetup -O BACK-FILE -n "$1" 2>/dev/null | head -1); [ -n "$back" ] || { echo "$(basename "$0"): $1 is not an attached loop device" >&2; exit 1; } ;;
  esac
  SET_DIR=$(dirname "$back")
}
