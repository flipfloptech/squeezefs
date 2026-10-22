# Shared by the xfstests helpers (sourced): the SET behind a device string,
# and the ONE-DEVICE layout of that set.
#
# xfstests hands the helpers ONE block device per set (TEST_DEV, SCRATCH_DEV:
# loop devices; or a dm-flakey / dm-error / dm-log-writes target the test put
# over one; or a device of its own — dmhugedisk's 17 TB snapshot over
# dm-zero, a thin volume, a scsi_debug disk) and its crash-consistency tests
# cut, error or log THAT device. A set's state spans a metadata volume and
# two data volumes, so every byte of it must sit on that one device: the
# helpers carve it with dm-linear into three slices of stable names,
#
#   /dev/mapper/sqz-<set>-meta    SUITE_META_MIB                the metadata volume
#   /dev/mapper/sqz-<set>-d0/-d1  max(SUITE_DATA_GIB, the rest/2) the data volumes
#
# created over whatever device the test hands over at mkfs/mount/fsck time
# (re-pointed if they exist) and removed at umount, so the test's own dm
# device can be removed by its cleanup. Before this (2026-10-17) the data
# volumes were files BESIDE the metadata image, outside every cut: a
# dm-flakey test lost fsync'ed data to a promotion whose data-volume write
# survived while its metadata mapping was dropped. The staging ring is the
# WRITE CACHE in RAM (2026-10-21): no slice, no ext4, nothing under a cut —
# its fsync law promotes to the data volumes.
#
# The slices FOLLOW THE DEVICE (2026-10-22, the production shape: a set
# lives on raw nvmet namespaces, charter §1.2): the metadata slice is
# SUITE_META_MIB and the two data slices split the rest of the device, with
# SUITE_DATA_GIB as their floor — the loop sets are exactly the floor's
# size, so their layout is unchanged, and generic/620's 17 TB hugedisk
# gets two 8.5 TiB data volumes. ANY block device is a set device: a loop
# device keys its set by its backing image's directory (`set-test`,
# `set-scratch`, the logs beside the image); any other — a dm target that
# is no pass-through, a loop over a block device, a disk — by its kernel
# name (the dm name, else the basename), its logs under
# `$SUITE_SETS_DIR/<name>` (fstests.sh writes the run's directory into the
# helpers' env file), never /dev (devtmpfs).
#
# `suite_set_of <dev>` answers SET_DEV (the device string as handed over),
# SET_DIR (the set's logs) and SET_ID, plus the slice paths.
SUITE_META_MIB=${SUITE_META_MIB:-1024}
SUITE_DATA_GIB=${SUITE_DATA_GIB:-8}
SUITE_SETS_DIR=${SUITE_SETS_DIR:-}

suite_set_of() {
  SET_DEV=$1
  local dev=$1 back
  case "$dev" in /dev/mapper/sqz-*) echo "$(basename "$0"): $dev is one of the set's own slices, not a set device" >&2; exit 1 ;; esac
  # A device-mapper target that passes through to ONE device (dm-flakey,
  # dm-log-writes, dm-delay, dm-dust, dm-linear, snapshot-origin — xfstests
  # puts them over the loop): the set is that device's, level by level.
  # dm-error's table names none (`0 N error`): the slices already standing
  # over it name the set. Any other target (dmhugedisk's snapshot over
  # dm-zero, a thin volume) is a set device of its own.
  local hops=0 tbl type mm
  while [[ $dev == /dev/mapper/* || $dev == /dev/dm-* ]] && [ "$hops" -lt 8 ]; do
    tbl=$(dmsetup table "$dev" 2>&1 | head -1)
    read -r _ _ type mm _ <<< "$tbl"
    case "$type" in
      linear|flakey|delay|log-writes|dust|snapshot-origin)
        [[ $mm =~ ^[0-9]+:[0-9]+$ ]] || { echo "$(basename "$0"): $dev: a $type target whose table names no device ($tbl)" >&2; exit 1; }
        dev=/dev/$(basename "$(readlink -f "/sys/dev/block/$mm")"); hops=$((hops + 1)) ;;
      error)
        local id; id=$(suite_set_id_over "$dev") || { echo "$(basename "$0"): $dev is a dm-error target with no set's slices over it ($tbl)" >&2; exit 1; }
        SET_DIR=$(cat "/run/squeezefs-suites/$id.dir"); SET_ID=$id; suite_slice_paths; return 0 ;;
      *) break ;;
    esac
  done
  [ -b "$dev" ] || { echo "$(basename "$0"): $dev is not a block device" >&2; exit 1; }
  back=""
  case "$dev" in
    /dev/loop*) back=$(losetup -O BACK-FILE -n "$dev" 2>/dev/null | head -1); [ -n "$back" ] || { echo "$(basename "$0"): $dev is not an attached loop device" >&2; exit 1; } ;;
  esac
  if [ -n "$back" ] && [ ! -b "$back" ]; then
    SET_DIR=$(dirname "$back")
    SET_ID=$(basename "$SET_DIR")
  else
    # A raw set device (a loop over a block device included — generic/563):
    # keyed by its kernel name.
    SET_ID=$(suite_dev_name "$dev")
    [ -n "$SUITE_SETS_DIR" ] || { echo "$(basename "$0"): $dev is a raw block device and SUITE_SETS_DIR is unset: the harness's env file (${SQZ_SUITES_ENV:-/run/squeezefs-suites.env}) names where such a set keeps its logs" >&2; exit 1; }
    SET_DIR=$SUITE_SETS_DIR/$SET_ID
    mkdir -p "$SET_DIR"
  fi
  suite_slice_paths
  mkdir -p /run/squeezefs-suites && echo "$SET_DIR" > "/run/squeezefs-suites/$SET_ID.dir"
}

# A block device's kernel name: the dm name for a dm device, else the basename.
suite_dev_name() {
  local dev=$1 name=""
  case "$dev" in /dev/mapper/*|/dev/dm-*) name=$(dmsetup info -c --noheadings -o name "$dev" 2>/dev/null) ;; esac
  [ -n "$name" ] || name=$(basename "$dev")
  echo "$name"
}

suite_slice_paths() {
  SLICE_META=/dev/mapper/sqz-$SET_ID-meta
  SLICE_D0=/dev/mapper/sqz-$SET_ID-d0
  SLICE_D1=/dev/mapper/sqz-$SET_ID-d1
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

# Bytes the one device must hold: the three slices at the floor.
suite_set_bytes() { echo $(( (SUITE_META_MIB << 20) + 2 * (SUITE_DATA_GIB << 30) )); }

# The layout over a device of `$1` 512 B sectors: `<meta sectors> <data
# sectors>` (each data slice). The metadata slice is SUITE_META_MIB; the
# data slices split the rest, SUITE_DATA_GIB the floor, each rounded down
# to a 1 MiB multiple. Pure: the dry run's and the dm tables' one law.
suite_slice_layout() {
  local total=$1 ss=512 meta data floor
  meta=$(( (SUITE_META_MIB << 20) / ss )); floor=$(( (SUITE_DATA_GIB << 30) / ss ))
  [ "$total" -ge $(( meta + 2 * floor )) ] || return 1
  data=$(( (total - meta) / 2 / 2048 * 2048 ))
  [ "$data" -ge "$floor" ] || data=$floor
  echo "$meta $data"
}

# The slices over `$1` (created, or re-pointed when they stand already).
suite_slices_up() {
  local dev=$1 start=0 spec name len total meta data
  total=$(blockdev --getsz "$dev") || { echo "$(basename "$0"): $dev: no size" >&2; exit 1; }
  read -r meta data < <(suite_slice_layout "$total") || { echo "$(basename "$0"): $dev holds $((total * 512)) B; the set needs $(suite_set_bytes) B (SUITE_META_MIB + 2 × SUITE_DATA_GIB)" >&2; exit 1; }
  for spec in "meta:$meta" "d0:$data" "d1:$data"; do
    name=sqz-$SET_ID-${spec%%:*}; len=${spec#*:}
    if dmsetup info "$name" >/dev/null 2>&1; then
      dmsetup suspend "$name" && dmsetup load "$name" --table "0 $len linear $dev $start" && dmsetup resume "$name"
    else
      dmsetup create "$name" --table "0 $len linear $dev $start"
    fi || { echo "$(basename "$0"): slice $name over $dev failed" >&2; exit 1; }
    start=$((start + len))
  done
}

# Everything down: the three slices removed (a holder still closing gets
# dmsetup's retries), so the test's own dm device above them can go.
suite_slices_down() {
  local s
  for s in d1 d0 meta; do dmsetup info "sqz-$SET_ID-$s" >/dev/null 2>&1 && dmsetup remove --retry "sqz-$SET_ID-$s" 2>/dev/null; done
  return 0
}
