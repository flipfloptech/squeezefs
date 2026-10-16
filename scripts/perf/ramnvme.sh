#!/usr/bin/env bash
# ramnvme.sh up|down|status — RAM-backed NVMe namespaces (no device ceiling: the software's own).
# One brd RAM disk cut into 2 MDT + 2 OST partitions, each shared by the product's own nvmet tooling
# (`squeezefs nvmeof share --transport loop`, one namespace per subsystem) and connected with
# `squeezefs nvmeof connect --transport loop`, so a mount sees real /dev/nvmeXnY namespaces
# (passthrough, PR) at RAM speed.
# brd, not null_blk memory_backed=1: null_blk copies every transfer under the device's one spinlock
# (null_handle_rq → nullb->lock); brd reads are lockless (xarray under RCU) and its bio path runs in
# the submitter's context, so the namespace scales with the submitting CPUs. brd takes RAM per page
# written and gives it back at rmmod.
#   up      prints `mdt0=/dev/nvmeXnY …` and writes them to $STATE/devs (perf_mount.sh --ram reads it)
#   down    disconnect, unshare, rmmod brd, and verify no subsystem/port/namespace/module is left
#   status  the devices and the configfs entries that are ours
# RAMNVME_MDT_GIB (4) × 2 + RAMNVME_OST_GIB (14) × 2; the binary: RAMNVME_BIN (zig-out-perf's dist build).
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
BIN=${RAMNVME_BIN:-$REPO/zig-out-perf/bin/squeezefs}
STATE=/run/sqz-ramnvme
MDT_GIB=${RAMNVME_MDT_GIB:-4} OST_GIB=${RAMNVME_OST_GIB:-14}
ROLES=(mdt0 mdt1 ost0 ost1)
NQN=nqn.2026-10.io.squeezefs:perfram
CFS=/sys/kernel/config/nvmet
# The ledger beside our state, never the host's /var/lib/squeezefs/nvmeof.
export SQUEEZEFS_NVMEOF_STATE_DIR=$STATE/ledger
die() { echo "ramnvme: $*" >&2; exit 1; }
[ "$(id -u)" = 0 ] || die "root is required"

residue() {   # what of ours is still in the kernel
  local r=""
  for s in "$CFS"/subsystems/"$NQN"-*; do [ -e "$s" ] && r="$r subsystem:$(basename "$s")"; done
  for p in "$CFS"/ports/*/subsystems/"$NQN"-*; do [ -e "$p" ] && r="$r port-link:$p"; done
  grep -ls "^$NQN-" /sys/class/nvme/nvme*/subsysnqn 2>/dev/null | while read -r f; do echo -n " controller:$(basename "$(dirname "$f")")"; done
  [ -d /sys/module/brd ] && r="$r module:brd"
  for p in $(ls "$CFS/ports" 2>/dev/null); do grep -qsx "$p" "$STATE/ports.before" || r="$r port:$p"; done
  echo -n "$r"
}

up() {
  [ -e "$STATE/devs" ] && die "already up ($(tr '\n' ' ' < "$STATE/devs")); down first"
  [ -d /sys/module/brd ] && die "brd is loaded by someone else; refusing to take it"
  [ -x "$BIN" ] || die "$BIN is absent (build: zig build -Dprofile=dist --prefix zig-out-perf dist)"
  mkdir -p "$STATE"
  # What was here before us: down unloads only the modules and removes only the ports we added.
  local m; for m in nvme_loop nvmet; do [ -d "/sys/module/$m" ] || echo "$m" >> "$STATE/modules.ours"; done
  ls "$CFS/ports" > "$STATE/ports.before" 2>/dev/null || : > "$STATE/ports.before"
  modprobe -a nvmet nvme_loop
  local kib=$(((2 * MDT_GIB + 2 * OST_GIB + 1) * 1024 * 1024))
  modprobe brd rd_nr=1 rd_size=$kib max_part=8
  printf 'label: gpt\nsize=%sGiB\nsize=%sGiB\nsize=%sGiB\nsize=%sGiB\n' "$MDT_GIB" "$MDT_GIB" "$OST_GIB" "$OST_GIB" \
    | sfdisk -q /dev/ram0 >/dev/null
  udevadm settle
  local i=1 role dev
  : > "$STATE/devs.tmp"
  for role in "${ROLES[@]}"; do
    [ -b /dev/ram0p$i ] || die "/dev/ram0p$i did not appear"
    "$BIN" nvmeof share --transport loop --subnqn "$NQN-$role" /dev/ram0p$i > "$STATE/share-$role.log" 2>&1 \
      || { cat "$STATE/share-$role.log" >&2; die "share $role failed"; }
    "$BIN" nvmeof connect --transport loop "$NQN-$role" > "$STATE/connect-$role.log" 2>&1 \
      || { cat "$STATE/connect-$role.log" >&2; die "connect $role failed"; }
    dev=$(awk '$1 == "device" {print $2}' "$STATE/connect-$role.log")
    [ -b "$dev" ] || die "connect $role named no block device ($(cat "$STATE/connect-$role.log"))"
    echo "$role=$dev" >> "$STATE/devs.tmp"
    i=$((i + 1))
  done
  mv "$STATE/devs.tmp" "$STATE/devs"
  cat "$STATE/devs"
}

down() {
  local role
  for role in "${ROLES[@]}"; do
    if grep -qs "^$NQN-$role\$" /sys/class/nvme/nvme*/subsysnqn; then
      "$BIN" nvmeof disconnect "$NQN-$role" >/dev/null || echo "ramnvme: disconnect $role failed" >&2
    fi
    if [ -e "$CFS/subsystems/$NQN-$role" ]; then
      "$BIN" nvmeof unshare "$NQN-$role" >/dev/null || echo "ramnvme: unshare $role failed" >&2
    fi
  done
  # udev's probe of the vanished /dev/ram* nodes autoloads brd again (block-major-1): repeat until gone.
  local i
  for i in 1 2 3 4 5; do
    [ -d /sys/module/brd ] || break
    udevadm settle; rmmod brd 2>/dev/null || true; udevadm settle; sleep 1
  done
  local p
  for p in $(ls "$CFS/ports" 2>/dev/null); do
    [ -f "$STATE/ports.before" ] || break
    grep -qx "$p" "$STATE/ports.before" 2>/dev/null && continue
    [ -z "$(ls "$CFS/ports/$p/subsystems")" ] && rmdir "$CFS/ports/$p" && echo "ramnvme: removed our empty port $p" >&2
  done
  for p in $(cat "$STATE/modules.ours" 2>/dev/null); do modprobe -r "$p" || echo "ramnvme: modprobe -r $p failed" >&2; done
  local r; r=$(residue)
  [ -z "$r" ] || die "left behind:$r"
  rm -rf "$STATE"
  echo "ramnvme: down (no subsystem, port link, controller or brd left)"
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  status) cat "$STATE/devs" 2>/dev/null || echo "not up"; echo "ours in the kernel:$(residue)" ;;
  *) die "usage: ramnvme.sh up|down|status" ;;
esac
